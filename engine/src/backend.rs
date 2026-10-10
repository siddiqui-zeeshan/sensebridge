//! Drives the real DualSense over Bluetooth.
//!
//! Report mapping follows awalol/DS5Dongle:
//!
//! * input: Bluetooth 0x31 payload to USB input report 0x01
//! * output: USB output report 0x02 to Bluetooth 0x31 with a CRC-32
//! * audio: channels 3 and 4 to Bluetooth 0x32 haptics reports

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::convert::{to_s8, Decimator, Frame};
use crate::device::Backend;
use crate::hid::{
    self, HidPort, HidSystem, BLUETOOTH_INPUT_LEN, HAPTICS_PACKET, HAPTICS_RATE, NEUTRAL_INPUT,
    PID_DUALSENSE_EDGE, USB_INPUT_LEN,
};
use crate::server::Log;

/// Calibration, pairing and firmware reports never change, so they are read once.
/// Reading calibration (0x05) is also what switches the controller to full reports.
const STATIC_FEATURES: [u8; 3] = [0x05, 0x09, 0x20];
const CALIBRATION: u8 = 0x05;
pub const MIN_BUFFER: u8 = 16;
const RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// No DualSense is connected.
    Absent,
    /// The controller is on a USB cable, where games already get haptics.
    Wired,
    /// Windows would not let us open it.
    Busy,
}

impl Unavailable {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Wired => "wired",
            Self::Busy => "busy",
        }
    }
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Absent => "no DualSense found",
            Self::Wired => "the controller is on a USB cable",
            Self::Busy => "could not open the controller",
        })
    }
}

#[derive(Default)]
pub struct Counts {
    pub input: AtomicU64,
    pub output: AtomicU64,
    pub haptics: AtomicU64,
    pub set_feature_dropped: AtomicU64,
    pub reconnects: AtomicU64,
}

struct Audio {
    decimator: Decimator,
    /// Converted samples waiting to fill a haptics report.
    pending: Vec<u8>,
}

type InputSink = Arc<dyn Fn(&[u8; USB_INPUT_LEN]) + Send + Sync>;

pub struct BluetoothBackend {
    system: Arc<dyn HidSystem>,
    log: Log,
    /// Write side; `None` while the controller is away.
    writer: Mutex<Option<Box<dyn HidPort>>>,
    /// Read side, handed to the reader thread by `start`.
    reader: Mutex<Option<Box<dyn HidPort>>>,
    pub edge: bool,
    input_len: usize,
    input: Mutex<Option<[u8; USB_INPUT_LEN]>>,
    on_input: RwLock<Option<InputSink>>,
    features: Mutex<HashMap<u8, Vec<u8>>>,
    audio: Mutex<Audio>,
    gain: AtomicU32,
    buffer_len: AtomicU8,
    /// Loudest haptics sample since last read, in 1/1000.
    peak: AtomicU32,
    seq: AtomicU8,
    counter: AtomicU8,
    /// Set once serving: from then on a USB DualSense in the list is our own.
    serving: AtomicBool,
    running: AtomicBool,
    pub counts: Counts,
}

struct Opened {
    writer: Box<dyn HidPort>,
    reader: Box<dyn HidPort>,
    edge: bool,
    input_len: usize,
}

fn open(system: &dyn HidSystem, serving: bool) -> Result<Opened, Unavailable> {
    let found = system.find_controllers();
    // Never pick a USB one: once we are serving, that is our own virtual controller.
    let Some(bluetooth) = found.iter().find(|c| c.input_len >= BLUETOOTH_INPUT_LEN) else {
        return Err(if found.is_empty() || serving {
            Unavailable::Absent
        } else {
            Unavailable::Wired
        });
    };
    match (system.open(&bluetooth.path), system.open(&bluetooth.path)) {
        (Some(writer), Some(reader)) => Ok(Opened {
            writer,
            reader,
            edge: bluetooth.product_id == PID_DUALSENSE_EDGE,
            input_len: bluetooth.input_len,
        }),
        _ => Err(Unavailable::Busy),
    }
}

impl BluetoothBackend {
    pub fn new(
        system: Arc<dyn HidSystem>,
        buffer_len: u8,
        gain: f32,
        log: Log,
    ) -> Result<Arc<Self>, Unavailable> {
        let opened = open(system.as_ref(), false)?;
        Ok(Arc::new(Self {
            system,
            log,
            writer: Mutex::new(Some(opened.writer)),
            reader: Mutex::new(Some(opened.reader)),
            edge: opened.edge,
            input_len: opened.input_len,
            input: Mutex::new(None),
            on_input: RwLock::new(None),
            features: Mutex::default(),
            audio: Mutex::new(Audio { decimator: Decimator::default(), pending: Vec::new() }),
            gain: AtomicU32::new(gain.to_bits()),
            buffer_len: AtomicU8::new(buffer_len.max(MIN_BUFFER)),
            peak: AtomicU32::new(0),
            seq: AtomicU8::new(0),
            counter: AtomicU8::new(0),
            serving: AtomicBool::new(false),
            running: AtomicBool::new(true),
            counts: Counts::default(),
        }))
    }

    pub fn connected(&self) -> bool {
        self.writer.lock().unwrap().is_some()
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    /// Controller haptics queue depth: lower is faster; below 16 it stops playing cleanly.
    pub fn set_buffer(&self, buffer_len: u8) {
        self.buffer_len.store(buffer_len.max(MIN_BUFFER), Ordering::Relaxed);
    }

    /// Loudest haptics sample sent since the last call, 0 to 1.
    pub fn take_peak(&self) -> f32 {
        self.peak.swap(0, Ordering::Relaxed) as f32 / 1000.0
    }

    /// (percent, charging), or `None` until the controller has reported.
    pub fn battery(&self) -> Option<(u8, bool)> {
        self.input.lock().unwrap().as_ref().map(hid::battery)
    }

    pub fn set_input_sink(&self, sink: InputSink) {
        *self.on_input.write().unwrap() = Some(sink);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        *self.writer.lock().unwrap() = None;
        // The sink usually holds the server, which holds this backend: break the loop.
        *self.on_input.write().unwrap() = None;
    }

    /// Read the static feature reports and start forwarding input.
    pub fn start(self: &Arc<Self>) {
        for report_id in STATIC_FEATURES {
            if self.get_feature(report_id, 0).is_none() {
                (self.log)(format!("feature report {report_id:#04x} unavailable"));
            }
        }
        self.serving.store(true, Ordering::Relaxed);
        let Some(reader) = self.reader.lock().unwrap().take() else { return };
        let backend = Arc::clone(self);
        thread::spawn(move || backend.read_loop(reader));
    }

    fn publish(&self, report: Option<[u8; USB_INPUT_LEN]>) {
        *self.input.lock().unwrap() = report;
        if let Some(sink) = self.on_input.read().unwrap().as_ref() {
            sink(report.as_ref().unwrap_or(&NEUTRAL_INPUT));
        }
    }

    fn read_loop(&self, mut reader: Box<dyn HidPort>) {
        let mut buf = vec![0u8; self.input_len];
        loop {
            while self.running.load(Ordering::Relaxed) && reader.read(&mut buf) {
                // Bit 1 of the tag byte marks a 0x31 report that carries a microphone
                // frame instead of controller state (per DS5Dongle); those must not
                // reach the host.
                if buf[0] != 0x31 || buf[1] & 0x02 != 0 {
                    continue;
                }
                self.counts.input.fetch_add(1, Ordering::Relaxed);
                self.publish(Some(hid::input_to_usb(&buf)));
            }
            if !self.running.load(Ordering::Relaxed) {
                return;
            }
            (self.log)("controller lost; waiting for it to come back".to_string());
            // Release everything, or the host keeps seeing whatever was held when the
            // link died.
            self.publish(None);
            *self.writer.lock().unwrap() = None;
            drop(reader);
            let opened = loop {
                if !self.running.load(Ordering::Relaxed) {
                    return;
                }
                match open(self.system.as_ref(), true) {
                    Ok(opened) => break opened,
                    Err(_) => thread::sleep(RETRY),
                }
            };
            // A freshly connected controller sends short reports until calibration is read.
            let _ = opened.writer.get_feature(CALIBRATION);
            *self.writer.lock().unwrap() = Some(opened.writer);
            reader = opened.reader;
            self.counts.reconnects.fetch_add(1, Ordering::Relaxed);
            (self.log)("controller is back".to_string());
        }
    }

    fn write(&self, report: &[u8]) -> bool {
        match self.writer.lock().unwrap().as_ref() {
            Some(port) => port.write(report),
            None => false,
        }
    }

    fn send_haptics(&self, packet: &[u8; HAPTICS_PACKET]) {
        let loudest = packet.iter().map(|&b| (b as i8).unsigned_abs()).max().unwrap_or(0);
        self.peak.fetch_max(loudest as u32 * 1000 / 127, Ordering::Relaxed);
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let buffer_len = self.buffer_len.load(Ordering::Relaxed);
        self.write(&hid::haptics_report(packet, buffer_len, counter));
        self.counts.haptics.fetch_add(1, Ordering::Relaxed);
    }

    /// Play a short tone straight to the controller, bypassing the host.
    pub fn test_buzz(self: &Arc<Self>) {
        const HZ: f32 = 160.0;
        const SECONDS: f32 = 0.35;
        let backend = Arc::clone(self);
        thread::spawn(move || {
            let frames = HAPTICS_PACKET / 2;
            let period = Duration::from_secs_f64(frames as f64 / HAPTICS_RATE as f64);
            let mut due = Instant::now();
            for packet_index in 0..(SECONDS * HAPTICS_RATE as f32 / frames as f32) as usize {
                let gain = backend.gain();
                let mut packet = [0u8; HAPTICS_PACKET];
                for i in 0..frames {
                    let n = (packet_index * frames + i) as f32;
                    let sample =
                        0.8 * (2.0 * std::f32::consts::PI * HZ * n / HAPTICS_RATE as f32).sin();
                    let value = to_s8(sample, gain) as u8;
                    packet[2 * i] = value;
                    packet[2 * i + 1] = value;
                }
                backend.send_haptics(&packet);
                due += period;
                thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        });
    }
}

impl Backend for BluetoothBackend {
    fn get_feature(&self, report_id: u8, length: usize) -> Option<Vec<u8>> {
        let cached = self.features.lock().unwrap().get(&report_id).cloned();
        let mut raw = match cached {
            Some(raw) => raw,
            None => {
                let raw = self.writer.lock().unwrap().as_ref()?.get_feature(report_id)?;
                if STATIC_FEATURES.contains(&report_id) {
                    self.features.lock().unwrap().insert(report_id, raw.clone());
                }
                raw
            }
        };
        if length > 0 {
            raw.truncate(length);
        }
        Some(raw)
    }

    fn set_feature(&self, report_id: u8, data: &[u8]) {
        // Needs the per-report Bluetooth length and CRC; not needed for play, so not sent.
        self.counts.set_feature_dropped.fetch_add(1, Ordering::Relaxed);
        (self.log)(format!("dropped SET_REPORT feature {report_id:#04x} ({} bytes)", data.len()));
    }

    fn output_report(&self, data: &[u8]) {
        if data.len() < 2 || data[0] != 0x02 {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.write(&hid::output_to_bluetooth(data, seq));
        self.counts.output.fetch_add(1, Ordering::Relaxed);
    }

    fn audio_out(&self, pcm: &[u8]) {
        let frames: Vec<Frame> = pcm
            .chunks_exact(8)
            .map(|f| {
                [
                    i16::from_le_bytes([f[4], f[5]]) as f32 / 32768.0,
                    i16::from_le_bytes([f[6], f[7]]) as f32 / 32768.0,
                ]
            })
            .collect();
        let gain = self.gain();
        let mut audio = self.audio.lock().unwrap();
        for frame in audio.decimator.process(&frames) {
            audio.pending.push(to_s8(frame[0], gain) as u8);
            audio.pending.push(to_s8(frame[1], gain) as u8);
        }
        while audio.pending.len() >= HAPTICS_PACKET {
            let packet: [u8; HAPTICS_PACKET] =
                audio.pending[..HAPTICS_PACKET].try_into().unwrap();
            audio.pending.drain(..HAPTICS_PACKET);
            self.send_haptics(&packet);
        }
    }

    fn set_speaker_active(&self, active: bool) {
        if !active {
            let mut audio = self.audio.lock().unwrap();
            audio.pending.clear();
            audio.decimator = Decimator::default();
        }
    }

    fn latest_input(&self) -> Vec<u8> {
        self.input.lock().unwrap().unwrap_or(NEUTRAL_INPUT).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Found;
    use std::sync::atomic::AtomicUsize;

    /// A controller whose link dies after a few input reports, then comes back.
    struct FakeSystem {
        present: AtomicBool,
        reads_left: AtomicUsize,
        /// Every port opened: (generation, path).
        opened: Mutex<Vec<(usize, String)>>,
        /// Generation of the port each write and calibration read went to.
        writes: Arc<Mutex<Vec<usize>>>,
        calibration_reads: Arc<Mutex<Vec<usize>>>,
    }

    struct FakePort {
        generation: usize,
        system: Arc<FakeSystem>,
    }

    impl HidPort for FakePort {
        fn read(&self, buf: &mut [u8]) -> bool {
            thread::sleep(Duration::from_millis(1));
            // The first link (ports 1 and 2) dies; later ones stay up.
            if self.generation <= 2
                && self
                    .system
                    .reads_left
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_err()
            {
                return false;
            }
            buf[0] = 0x31;
            buf[1] = 0;
            buf[2] = if self.generation <= 2 { 0x55 } else { 0x66 };
            true
        }

        fn write(&self, _report: &[u8]) -> bool {
            self.system.writes.lock().unwrap().push(self.generation);
            true
        }

        fn get_feature(&self, report_id: u8) -> Option<Vec<u8>> {
            if report_id == CALIBRATION {
                self.system.calibration_reads.lock().unwrap().push(self.generation);
            }
            Some(vec![report_id; 64])
        }
    }

    struct Shared(Arc<FakeSystem>);

    impl HidSystem for Shared {
        fn find_controllers(&self) -> Vec<Found> {
            // The virtual wired controller is always listed, and listed first.
            let mut found = vec![Found {
                path: "virtual".into(),
                product_id: 0x0CE6,
                input_len: 64,
            }];
            if self.0.present.load(Ordering::Relaxed) {
                found.push(Found { path: "bluetooth".into(), product_id: 0x0CE6, input_len: 78 });
            }
            found
        }

        fn open(&self, path: &str) -> Option<Box<dyn HidPort>> {
            let mut opened = self.0.opened.lock().unwrap();
            let generation = opened.len() + 1;
            opened.push((generation, path.to_string()));
            Some(Box::new(FakePort { generation, system: Arc::clone(&self.0) }))
        }
    }

    struct Rig {
        system: Arc<FakeSystem>,
        backend: Arc<BluetoothBackend>,
        seen: Arc<Mutex<Vec<[u8; USB_INPUT_LEN]>>>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.backend.stop();
        }
    }

    fn fake_system() -> Arc<FakeSystem> {
        Arc::new(FakeSystem {
            present: AtomicBool::new(true),
            reads_left: AtomicUsize::new(5),
            opened: Mutex::default(),
            writes: Arc::default(),
            calibration_reads: Arc::default(),
        })
    }

    fn rig() -> Rig {
        let system = fake_system();
        let backend = BluetoothBackend::new(
            Arc::new(Shared(Arc::clone(&system))),
            16,
            1.0,
            Arc::new(|_| {}),
        )
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        backend.set_input_sink(Arc::new(move |report| sink.lock().unwrap().push(*report)));
        Rig { system, backend, seen }
    }

    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    impl Rig {
        fn saw_new_link(&self) -> bool {
            wait_for(|| self.seen.lock().unwrap().iter().any(|r| r[1] == 0x66))
        }
    }

    #[test]
    fn input_resumes_after_the_link_drops() {
        let rig = rig();
        rig.backend.start();
        assert!(rig.saw_new_link(), "no input from the controller after it came back");
        assert_eq!(rig.backend.counts.reconnects.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn host_sees_released_buttons_while_the_controller_is_away() {
        let rig = rig();
        rig.system.present.store(false, Ordering::Relaxed);
        rig.backend.start();
        assert!(
            wait_for(|| rig.seen.lock().unwrap().contains(&NEUTRAL_INPUT)),
            "held buttons would stay held"
        );
        assert_eq!(rig.backend.latest_input(), NEUTRAL_INPUT.to_vec());
        assert!(!rig.backend.connected());
        rig.backend.output_report(&[0x02; 48]); // must not panic with no controller
        rig.system.present.store(true, Ordering::Relaxed);
        assert!(rig.saw_new_link());
        assert!(rig.backend.connected());
    }

    #[test]
    fn reconnect_switches_the_controller_back_to_full_reports() {
        let rig = rig();
        rig.backend.start();
        assert!(rig.saw_new_link());
        let reads = rig.system.calibration_reads.lock().unwrap();
        assert!(reads.iter().any(|&generation| generation > 2), "calibration not re-read");
    }

    #[test]
    fn never_opens_the_virtual_wired_controller() {
        let rig = rig();
        rig.backend.start();
        assert!(rig.saw_new_link());
        assert!(rig.system.opened.lock().unwrap().iter().all(|(_, path)| path == "bluetooth"));
    }

    #[test]
    fn writes_go_to_the_new_link() {
        let rig = rig();
        rig.backend.start();
        assert!(rig.saw_new_link());
        rig.system.writes.lock().unwrap().clear();
        rig.backend.output_report(&[0x02; 48]);
        assert!(rig.system.writes.lock().unwrap().iter().all(|&generation| generation > 2));
        assert_eq!(rig.system.writes.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_cable_only_controller_is_reported_as_wired() {
        let system = fake_system();
        system.present.store(false, Ordering::Relaxed);
        let result = BluetoothBackend::new(Arc::new(Shared(system)), 16, 1.0, Arc::new(|_| {}));
        assert_eq!(result.err(), Some(Unavailable::Wired));
    }

    #[test]
    fn haptics_channels_become_haptics_reports() {
        let rig = rig();
        // 48 kHz, 4 channels: 16 input frames per output frame, 32 output frames per
        // report, so 512 frames (after the filter fills) make one report.
        let mut pcm = Vec::new();
        for n in 0..4800 {
            let v = ((2.0 * std::f32::consts::PI * 160.0 * n as f32 / 48000.0).sin() * 16000.0)
                as i16;
            for channel in 0..4 {
                pcm.extend_from_slice(&(if channel >= 2 { v } else { 0 }).to_le_bytes());
            }
        }
        rig.backend.audio_out(&pcm);
        assert_eq!(rig.backend.counts.haptics.load(Ordering::Relaxed), 9);
        let peak = rig.backend.take_peak();
        assert!((0.4..0.6).contains(&peak), "peak {peak}");
        assert_eq!(rig.backend.take_peak(), 0.0);
    }
}
