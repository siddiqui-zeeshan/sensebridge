//! Drives the usbip-win2 command-line client (<https://github.com/vadimgrn/usbip-win2>),
//! whose driver is what makes the served device appear in Windows.

use std::env;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

pub const DOWNLOAD_URL: &str = "https://github.com/vadimgrn/usbip-win2/releases/latest";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Path of usbip.exe, or `None` when usbip-win2 is not installed.
pub fn find_usbip() -> Option<PathBuf> {
    let installed = ["ProgramFiles", "ProgramW6432"]
        .iter()
        .filter_map(|var| env::var_os(var))
        .map(|root| PathBuf::from(root).join("USBip").join("usbip.exe"));
    let on_path = env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).map(|dir| dir.join("usbip.exe")).collect::<Vec<_>>())
        .unwrap_or_default();
    installed.chain(on_path).find(|path| path.is_file())
}

/// Port number from "successfully attached to port 1".
pub fn parse_attached_port(text: &str) -> Option<u32> {
    let rest = &text[text.find("attached to port")? + "attached to port".len()..];
    let digits: String =
        rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn run(args: &[&str]) -> Result<String, String> {
    let exe = find_usbip().ok_or("usbip-win2 is not installed")?;
    let output = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| error.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string();
    if output.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// Attach the device served on 127.0.0.1. Returns the usbip port it landed on.
pub fn attach(tcp_port: u16, busid: &str) -> Result<u32, String> {
    let tcp_port = tcp_port.to_string();
    let mut args = vec!["attach", "-r", "127.0.0.1", "-b", busid];
    if tcp_port != "3240" {
        args.splice(0..0, ["--tcp-port", tcp_port.as_str()]);
    }
    let text = run(&args)?;
    parse_attached_port(&text).ok_or(text)
}

pub fn detach(port: u32) {
    let _ = run(&["detach", "-p", &port.to_string()]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_port_from_the_client_output() {
        assert_eq!(parse_attached_port("successfully attached to port 1"), Some(1));
        assert_eq!(parse_attached_port("x\nsuccessfully attached to port 12\n"), Some(12));
        assert_eq!(parse_attached_port("usbip: error: device busy"), None);
    }
}
