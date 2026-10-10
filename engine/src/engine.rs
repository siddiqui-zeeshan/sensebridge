//! Ties it together: wait for a Bluetooth DualSense, serve it as a wired one, attach
//! that to Windows, keep it attached, and report what is going on.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::backend::{BluetoothBackend, MIN_BUFFER};
use crate::device::DualSenseDevice;
use crate::hid::{HidSystem, Windows};
use crate::server::{Log, Server};
use crate::usbipctl;

const STATUS_EVERY: Duration = Duration::from_millis(100);
const ATTACH_RETRY: Duration = Duration::from_secs(3);
const CONTROLLER_RETRY: Duration = Duration::from_secs(1);
const PORT_RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// TCP port the USB/IP server listens on (loopback only).
    pub port: u16,
    /// Controller haptics queue depth, 16 (fastest) to 255.
    pub buffer: u8,
    /// Haptics strength; 1.0 sends what the game sent.
    pub gain: f32,
    /// Attach to Windows through usbip-win2. Off leaves that to the user.
    pub attach: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { port: 3240, buffer: MIN_BUFFER, gain: 1.0, attach: true }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    /// The real controller is connected over Bluetooth.
    pub controller: bool,
    /// Windows has the virtual controller attached.
    pub attached: bool,
    /// Haptics reports per second going to the controller.
    pub haptics_rate: u32,
    /// Trigger, light and rumble reports per second.
    pub output_rate: u32,
    /// Loudest haptics sample since the last status, 0 to 1.
    pub level: f32,
    pub battery: Option<u8>,
    pub charging: Option<bool>,
    pub reconnects: u64,
    pub edge: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// `state` is one of: waiting (reason absent, wired or busy), no_driver,
    /// attaching (reason empty or failed), ready, error, stopped.
    State { state: String, reason: String, detail: String },
    Status(Status),
    Log { text: String },
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

struct Shared {
    quit: AtomicBool,
    gain: AtomicU32,
    buffer: AtomicU8,
    backend: Mutex<Option<Arc<BluetoothBackend>>>,
    last_state: Mutex<Option<Event>>,
    emit: Emit,
}

impl Shared {
    fn state(&self, state: &str, reason: &str, detail: &str) {
        let event = Event::State {
            state: state.to_string(),
            reason: reason.to_string(),
            detail: detail.to_string(),
        };
        let mut last = self.last_state.lock().unwrap();
        if last.as_ref() != Some(&event) {
            *last = Some(event.clone());
            drop(last);
            (self.emit)(event);
        }
    }

    fn quitting(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    /// Sleep for `duration`, waking early to quit. Returns whether to quit.
    fn pause(&self, duration: Duration) -> bool {
        let end = Instant::now() + duration;
        while !self.quitting() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(Duration::from_millis(50)));
        }
        true
    }
}

/// A running engine. Dropping it stops and detaches.
pub struct Engine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(config: Config, emit: Emit) -> Self {
        Self::start_with(config, emit, Arc::new(Windows))
    }

    pub fn start_with(config: Config, emit: Emit, system: Arc<dyn HidSystem>) -> Self {
        let shared = Arc::new(Shared {
            quit: AtomicBool::new(false),
            gain: AtomicU32::new(config.gain.to_bits()),
            buffer: AtomicU8::new(config.buffer),
            backend: Mutex::new(None),
            last_state: Mutex::new(None),
            emit,
        });
        let thread = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                run(&shared, config, system);
                shared.state("stopped", "", "");
            })
        };
        Self { shared, thread: Some(thread) }
    }

    pub fn set_gain(&self, gain: f32) {
        let gain = gain.clamp(0.0, 4.0);
        self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
        if let Some(backend) = self.shared.backend.lock().unwrap().as_ref() {
            backend.set_gain(gain);
        }
    }

    pub fn set_buffer(&self, buffer: u8) {
        self.shared.buffer.store(buffer, Ordering::Relaxed);
        if let Some(backend) = self.shared.backend.lock().unwrap().as_ref() {
            backend.set_buffer(buffer);
        }
    }

    /// Buzz the controller briefly. Returns whether a controller was there to buzz.
    pub fn test_haptics(&self) -> bool {
        match self.shared.backend.lock().unwrap().as_ref() {
            Some(backend) if backend.connected() => {
                backend.test_buzz();
                true
            }
            _ => false,
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(shared: &Arc<Shared>, config: Config, system: Arc<dyn HidSystem>) {
    let log: Log = {
        let emit = Arc::clone(&shared.emit);
        Arc::new(move |text| emit(Event::Log { text }))
    };

    shared.state("waiting", "absent", "looking for the controller");
    let backend = loop {
        let gain = f32::from_bits(shared.gain.load(Ordering::Relaxed));
        let buffer = shared.buffer.load(Ordering::Relaxed);
        match BluetoothBackend::new(Arc::clone(&system), buffer, gain, Arc::clone(&log)) {
            Ok(backend) => break backend,
            Err(problem) => {
                shared.state("waiting", problem.reason(), &problem.to_string());
                if shared.pause(CONTROLLER_RETRY) {
                    return;
                }
            }
        }
    };
    *shared.backend.lock().unwrap() = Some(Arc::clone(&backend));

    let device = Arc::new(DualSenseDevice::new(backend.clone(), backend.edge));
    // The usual reason the port is taken is a second copy of this program. Keep saying
    // so and keep trying: it frees as soon as the other copy is closed.
    let server = loop {
        match Server::bind(Arc::clone(&device), config.port, Arc::clone(&log)) {
            Ok(server) => break server,
            Err(error) => {
                shared.state("error", "port", &format!("port {}: {error}", config.port));
                if shared.pause(PORT_RETRY) {
                    *shared.backend.lock().unwrap() = None;
                    backend.stop();
                    return;
                }
            }
        }
    };
    {
        let server = Arc::clone(&server);
        backend.set_input_sink(Arc::new(move |report| server.push_input(report)));
    }
    backend.start();
    server.serve();
    log(format!(
        "serving {:04X}:{:04X} on {}, bus id {}",
        DualSenseDevice::VENDOR_ID,
        device.product_id,
        server.address(),
        DualSenseDevice::BUSID
    ));

    let mut usbip_port = None;
    let mut last_attach_try: Option<Instant> = None;
    let mut last = (Instant::now(), 0u64, 0u64);
    while !shared.pause(STATUS_EVERY) {
        let attached = server.session().is_some();
        if attached {
            shared.state("ready", "", "");
        } else if !config.attach {
            let command = format!("usbip attach -r 127.0.0.1 -b {}", DualSenseDevice::BUSID);
            shared.state("attaching", "manual", &command);
        } else if last_attach_try.map_or(true, |at| at.elapsed() >= ATTACH_RETRY) {
            last_attach_try = Some(Instant::now());
            if usbipctl::find_usbip().is_none() {
                shared.state("no_driver", "", usbipctl::DOWNLOAD_URL);
            } else {
                shared.state("attaching", "", "");
                match usbipctl::attach(config.port, DualSenseDevice::BUSID) {
                    Ok(port) => usbip_port = Some(port),
                    Err(text) => shared.state("attaching", "failed", &text),
                }
            }
        }

        let now = Instant::now();
        let haptics = backend.counts.haptics.load(Ordering::Relaxed);
        let output = backend.counts.output.load(Ordering::Relaxed);
        let elapsed = (now - last.0).as_secs_f64().max(1e-3);
        let battery = backend.battery();
        (shared.emit)(Event::Status(Status {
            controller: backend.connected(),
            attached,
            haptics_rate: ((haptics - last.1) as f64 / elapsed).round() as u32,
            output_rate: ((output - last.2) as f64 / elapsed).round() as u32,
            level: backend.take_peak(),
            battery: battery.map(|b| b.0),
            charging: battery.map(|b| b.1),
            reconnects: backend.counts.reconnects.load(Ordering::Relaxed),
            edge: backend.edge,
        }));
        last = (now, haptics, output);
    }

    *shared.backend.lock().unwrap() = None;
    backend.stop();
    server.close();
    if let Some(port) = usbip_port {
        usbipctl::detach(port);
    }
}
