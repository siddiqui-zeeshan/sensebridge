//! "Open at sign-in": a per-user Run entry, no administrator rights needed.

use std::io;

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "SenseBridge";
/// Passed at sign-in so the app starts in the tray instead of opening its window.
pub const MINIMIZED_FLAG: &str = "--minimized";

fn command() -> io::Result<String> {
    Ok(format!("\"{}\" {MINIMIZED_FLAG}", std::env::current_exe()?.display()))
}

pub fn enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|key| key.get_value::<String, _>(VALUE))
        .is_ok()
}

pub fn set(enable: bool) -> io::Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
    if enable {
        key.set_value(VALUE, &command()?)
    } else {
        match key.delete_value(VALUE) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}
