//! Headless engine: the same thing the app runs, for terminals and scripts.
//!
//!   sensebridge-cli [--gain 1.0] [--buffer 16] [--port 3240] [--no-attach] [--json]
//!
//! Commands on standard input, one per line: `gain <x>`, `buffer <n>`, `test`, `quit`.
//! With --json every event is one JSON object per line.

use std::io::{self, BufRead};
use std::process::exit;
use std::sync::mpsc;
use std::sync::Arc;

use sensebridge_engine::{realtime, Config, Engine, Event};

const USAGE: &str =
    "usage: sensebridge-cli [--gain X] [--buffer N] [--port N] [--no-attach] [--json]";

fn value<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, flag: &str) -> T {
    args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        eprintln!("{flag} needs a value\n{USAGE}");
        exit(2)
    })
}

fn main() {
    let mut config = Config::default();
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--gain" => config.gain = value(&mut args, "--gain"),
            "--buffer" => config.buffer = value(&mut args, "--buffer"),
            "--port" => config.port = value(&mut args, "--port"),
            "--no-attach" => config.attach = false,
            "--json" => json = true,
            _ => {
                eprintln!("{USAGE}");
                exit(2)
            }
        }
    }

    realtime::boost();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let mut quiet_ticks = 0u32;
    let ticks = std::sync::Mutex::new(move || {
        quiet_ticks += 1;
        quiet_ticks % 50 == 0
    });
    let engine = Engine::start(
        config,
        Arc::new(move |event: Event| {
            if json {
                println!("{}", serde_json::to_string(&event).unwrap());
            } else {
                match &event {
                    Event::State { state, reason, detail } => {
                        println!("[{state}] {reason} {detail}")
                    }
                    Event::Log { text } => println!("{text}"),
                    // Ten a second is too chatty for a terminal: print every fifth second.
                    Event::Status(status) => {
                        if (ticks.lock().unwrap())() && status.attached {
                            println!(
                                "controller {} | haptics {}/s | output {}/s | level {:.2} | battery {}",
                                if status.controller { "connected" } else { "away" },
                                status.haptics_rate,
                                status.output_rate,
                                status.level,
                                status.battery.map_or("?".to_string(), |b| format!("{b}%")),
                            )
                        }
                    }
                }
            }
            if matches!(&event, Event::State { state, .. } if state == "stopped") {
                let _ = stopped_tx.send(());
            }
        }),
    );

    // Commands arrive on stdin; the main thread waits for the engine to stop.
    let (command_tx, command_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if command_tx.send(line).is_err() {
                return;
            }
        }
    });
    loop {
        if stopped_rx.try_recv().is_ok() {
            break;
        }
        let Ok(line) = command_rx.recv_timeout(std::time::Duration::from_millis(100)) else {
            continue;
        };
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("gain"), Some(v)) => {
                if let Ok(gain) = v.parse() {
                    engine.set_gain(gain)
                }
            }
            (Some("buffer"), Some(v)) => {
                if let Ok(buffer) = v.parse() {
                    engine.set_buffer(buffer)
                }
            }
            (Some("test"), _) => {
                engine.test_haptics();
            }
            (Some("quit"), _) => break,
            _ => {}
        }
    }
    engine.stop();
}
