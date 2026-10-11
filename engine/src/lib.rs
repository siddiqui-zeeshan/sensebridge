//! SenseBridge engine.
//!
//! A DualSense connected over Bluetooth gets no high-fidelity haptics from Windows
//! games: they send haptics as audio to the wired controller's USB audio device, which
//! does not exist over Bluetooth. This crate presents the Bluetooth controller to
//! Windows as a wired one, through a USB/IP server that the usbip-win2 driver attaches
//! to, and forwards what games send:
//!
//! * audio channels 3 and 4 become Bluetooth haptics reports (0x32),
//! * output reports (adaptive triggers, lights, rumble) become Bluetooth report 0x31,
//! * the controller's input reports go back as USB input reports.
//!
//! Credits: the haptics report layout is from egormanga/SAxense
//! (<https://apps.sdore.me/SAxense>); the USB descriptors and report mapping follow
//! awalol/DS5Dongle.

pub mod backend;
pub mod convert;
pub mod descriptors;
pub mod device;
pub mod engine;
pub mod hid;
pub mod pattern;
pub mod realtime;
pub mod server;
pub mod usbip;
pub mod usbipctl;

pub use engine::{Config, Engine, Event, Status};
