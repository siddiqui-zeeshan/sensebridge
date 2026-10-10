//! The engine's reported states, with a fake controller and no driver.

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sensebridge_engine::hid::{Found, HidPort, HidSystem};
use sensebridge_engine::{Config, Engine, Event};

/// A Bluetooth controller that is always there and sends a report every millisecond.
struct FakeController;

struct FakePort;

impl HidPort for FakePort {
    fn read(&self, buf: &mut [u8]) -> bool {
        thread::sleep(Duration::from_millis(1));
        buf[0] = 0x31;
        buf[1] = 0;
        true
    }
    fn write(&self, _report: &[u8]) -> bool {
        true
    }
    fn get_feature(&self, report_id: u8) -> Option<Vec<u8>> {
        Some(vec![report_id; 64])
    }
}

impl HidSystem for FakeController {
    fn find_controllers(&self) -> Vec<Found> {
        vec![Found { path: "bluetooth".into(), product_id: 0x0CE6, input_len: 78 }]
    }
    fn open(&self, _path: &str) -> Option<Box<dyn HidPort>> {
        Some(Box::new(FakePort))
    }
}

type States = Arc<Mutex<Vec<(String, String)>>>;

fn start(port: u16) -> (Engine, States) {
    let states: States = Arc::default();
    let seen = Arc::clone(&states);
    let engine = Engine::start_with(
        Config { port, attach: false, ..Config::default() },
        Arc::new(move |event| {
            if let Event::State { state, reason, .. } = event {
                seen.lock().unwrap().push((state, reason));
            }
        }),
        Arc::new(FakeController),
    );
    (engine, states)
}

fn wait_for(states: &States, state: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if states.lock().unwrap().iter().any(|(s, _)| s == state) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn serves_and_waits_to_be_attached() {
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (engine, states) = start(free);
    assert!(wait_for(&states, "attaching"), "states: {:?}", states.lock().unwrap());
    engine.stop();
    assert_eq!(states.lock().unwrap().last().unwrap().0, "stopped");
}

#[test]
fn can_be_turned_off_and_on_again() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    for round in 1..=3 {
        let (engine, states) = start(port);
        assert!(wait_for(&states, "attaching"), "round {round}: {:?}", states.lock().unwrap());
        assert!(
            !states.lock().unwrap().iter().any(|(s, _)| s == "error"),
            "round {round}: the engine's own earlier run still held the port: {:?}",
            states.lock().unwrap()
        );
        engine.stop();
    }
}

#[test]
fn a_busy_port_is_reported_and_retried_until_it_frees() {
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let (engine, states) = start(port);

    assert!(wait_for(&states, "error"), "states: {:?}", states.lock().unwrap());
    assert!(states.lock().unwrap().contains(&("error".to_string(), "port".to_string())));
    // The engine must keep saying what is wrong, not quietly give up: an engine that
    // stops on its own leaves the app showing "starting" for ever.
    thread::sleep(Duration::from_millis(300));
    assert!(
        !states.lock().unwrap().iter().any(|(s, _)| s == "stopped"),
        "engine gave up: {:?}",
        states.lock().unwrap()
    );

    drop(holder);
    assert!(wait_for(&states, "attaching"), "did not recover: {:?}", states.lock().unwrap());
    engine.stop();
}
