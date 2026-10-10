//! Raw HID access to a DualSense on Windows, and the Bluetooth haptics report.
//!
//! Device access goes through the [`HidSystem`] and [`HidPort`] traits so the logic
//! above it can be tested without a controller.

use std::ffi::c_void;
use std::ptr;

pub const VENDOR_ID: u16 = 0x054C;
pub const PID_DUALSENSE: u16 = 0x0CE6;
pub const PID_DUALSENSE_EDGE: u16 = 0x0DF2;

/// Input report length over Bluetooth; over USB (real or virtual) it is 64.
pub const BLUETOOTH_INPUT_LEN: usize = 78;

pub const HAPTICS_REPORT_LEN: usize = 142;
/// Bytes of audio per haptics report: 32 stereo frames of signed 8-bit at 3 kHz.
pub const HAPTICS_PACKET: usize = 64;
pub const HAPTICS_RATE: u32 = 3000;

/// One DualSense HID interface present on the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub product_id: u16,
    pub input_len: usize,
}

/// An open HID device.
pub trait HidPort: Send + Sync {
    /// Block for the next input report. `false` means the device is gone.
    fn read(&self, buf: &mut [u8]) -> bool;
    fn write(&self, report: &[u8]) -> bool;
    /// The feature report with this ID, report ID first, at the device's full length.
    fn get_feature(&self, report_id: u8) -> Option<Vec<u8>>;
}

pub trait HidSystem: Send + Sync {
    fn find_controllers(&self) -> Vec<Found>;
    fn open(&self, path: &str) -> Option<Box<dyn HidPort>>;
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32 (the zlib one) of `seed` followed by `data`.
pub fn crc32(seed: u8, data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in std::iter::once(&seed).chain(data) {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Bluetooth output reports carry a CRC over 0xA2 and the report up to the checksum.
const OUTPUT_CRC_SEED: u8 = 0xA2;

/// Haptics report 0x32 (layout from SAxense).
///
/// `buffer_len` is the controller's haptics queue depth and decides latency: SAxense
/// sends 255, about 195 ms; 16 gives about 25-45 ms and is the lowest that plays
/// cleanly. `counter` increments with every report.
pub fn haptics_report(
    pcm: &[u8; HAPTICS_PACKET],
    buffer_len: u8,
    counter: u8,
) -> [u8; HAPTICS_REPORT_LEN] {
    let mut r = [0u8; HAPTICS_REPORT_LEN];
    r[0] = 0x32;
    r[2] = 0x91;
    r[3] = 0x07;
    // 0x11 control block: field mask, then one byte per field, the last being queue depth.
    r[4] = 0xFE;
    r[9] = buffer_len;
    r[10] = counter;
    r[11] = 0x92;
    r[12] = HAPTICS_PACKET as u8;
    r[13..13 + HAPTICS_PACKET].copy_from_slice(pcm);
    let crc = crc32(OUTPUT_CRC_SEED, &r[..138]);
    r[138..142].copy_from_slice(&crc.to_le_bytes());
    r
}

/// USB output report 0x02 (triggers, lights, rumble) to Bluetooth report 0x31.
pub fn output_to_bluetooth(usb_report: &[u8], seq: u8) -> [u8; 78] {
    const STATE_LEN: usize = 47;
    let mut r = [0u8; 78];
    r[0] = 0x31;
    r[1] = (seq & 0x0F) << 4;
    r[2] = 0x10;
    let state = &usb_report[1..usb_report.len().min(1 + STATE_LEN)];
    r[3..3 + state.len()].copy_from_slice(state);
    let crc = crc32(OUTPUT_CRC_SEED, &r[..74]);
    r[74..78].copy_from_slice(&crc.to_le_bytes());
    r
}

pub const USB_INPUT_LEN: usize = 64;

/// What the host sees while the controller is away: sticks centred, nothing pressed.
pub const NEUTRAL_INPUT: [u8; USB_INPUT_LEN] = {
    let mut r = [0u8; USB_INPUT_LEN];
    r[0] = 0x01;
    r[1] = 0x80;
    r[2] = 0x80;
    r[3] = 0x80;
    r[4] = 0x80;
    r[8] = 0x08;
    r
};

/// Bluetooth input report 0x31 to USB input report 0x01.
pub fn input_to_usb(bt_report: &[u8]) -> [u8; USB_INPUT_LEN] {
    let mut r = [0u8; USB_INPUT_LEN];
    r[0] = 0x01;
    r[1..].copy_from_slice(&bt_report[2..65]);
    r
}

/// (percent, charging) from USB input report 0x01.
pub fn battery(usb_report: &[u8; USB_INPUT_LEN]) -> (u8, bool) {
    let status = usb_report[53];
    ((status & 0x0F).min(10) * 10, matches!(status >> 4, 1 | 2))
}

// ---------------------------------------------------------------------------------
// Windows implementation

type Handle = *mut c_void;
const INVALID_HANDLE: Handle = -1isize as Handle;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ_WRITE: u32 = 0x3;
const OPEN_EXISTING: u32 = 3;

#[repr(C)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

#[repr(C)]
struct HiddAttributes {
    size: u32,
    vendor_id: u16,
    product_id: u16,
    version: u16,
}

#[repr(C)]
#[derive(Default)]
struct HidpCaps {
    usage: u16,
    usage_page: u16,
    input_len: u16,
    output_len: u16,
    feature_len: u16,
    reserved: [u16; 17],
    rest: [u16; 10],
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn ReadFile(h: Handle, buf: *mut u8, len: u32, read: *mut u32, overlapped: *mut c_void)
        -> i32;
    fn WriteFile(
        h: Handle,
        buf: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(h: Handle) -> i32;
}

#[link(name = "hid")]
extern "system" {
    fn HidD_GetHidGuid(guid: *mut Guid);
    fn HidD_GetAttributes(h: Handle, attributes: *mut HiddAttributes) -> u8;
    fn HidD_GetPreparsedData(h: Handle, preparsed: *mut *mut c_void) -> u8;
    fn HidD_FreePreparsedData(preparsed: *mut c_void) -> u8;
    fn HidD_GetFeature(h: Handle, buf: *mut u8, len: u32) -> u8;
    fn HidP_GetCaps(preparsed: *mut c_void, caps: *mut HidpCaps) -> i32;
}

#[link(name = "cfgmgr32")]
extern "system" {
    fn CM_Get_Device_Interface_List_SizeW(
        len: *mut u32,
        class: *const Guid,
        device_id: *const u16,
        flags: u32,
    ) -> u32;
    fn CM_Get_Device_Interface_ListW(
        class: *const Guid,
        device_id: *const u16,
        buf: *mut u16,
        len: u32,
        flags: u32,
    ) -> u32;
}

struct OwnedHandle(Handle);

// A file handle may be used from any thread; reads and writes on it are serialised by
// the callers.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_handle(path: &str, access: u32) -> Option<OwnedHandle> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    (!h.is_null() && h != INVALID_HANDLE).then_some(OwnedHandle(h))
}

fn caps(h: &OwnedHandle) -> Option<HidpCaps> {
    let mut preparsed = ptr::null_mut();
    if unsafe { HidD_GetPreparsedData(h.0, &mut preparsed) } == 0 {
        return None;
    }
    let mut caps = HidpCaps::default();
    unsafe {
        HidP_GetCaps(preparsed, &mut caps);
        HidD_FreePreparsedData(preparsed);
    }
    Some(caps)
}

struct WindowsPort {
    handle: OwnedHandle,
    feature_len: usize,
}

impl HidPort for WindowsPort {
    fn read(&self, buf: &mut [u8]) -> bool {
        let mut read = 0u32;
        unsafe {
            ReadFile(self.handle.0, buf.as_mut_ptr(), buf.len() as u32, &mut read, ptr::null_mut())
                != 0
        }
    }

    fn write(&self, report: &[u8]) -> bool {
        let mut written = 0u32;
        unsafe {
            WriteFile(
                self.handle.0,
                report.as_ptr(),
                report.len() as u32,
                &mut written,
                ptr::null_mut(),
            ) != 0
        }
    }

    fn get_feature(&self, report_id: u8) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.feature_len.max(1)];
        buf[0] = report_id;
        let ok = unsafe { HidD_GetFeature(self.handle.0, buf.as_mut_ptr(), buf.len() as u32) };
        (ok != 0).then_some(buf)
    }
}

/// The real thing.
pub struct Windows;

impl HidSystem for Windows {
    fn find_controllers(&self) -> Vec<Found> {
        let mut guid = Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] };
        let mut size = 0u32;
        unsafe {
            HidD_GetHidGuid(&mut guid);
            CM_Get_Device_Interface_List_SizeW(&mut size, &guid, ptr::null(), 0);
        }
        let mut list = vec![0u16; size as usize];
        if size == 0
            || unsafe {
                CM_Get_Device_Interface_ListW(&guid, ptr::null(), list.as_mut_ptr(), size, 0)
            } != 0
        {
            return Vec::new();
        }
        let mut found = Vec::new();
        for wide in list.split(|&c| c == 0).filter(|p| !p.is_empty()) {
            let path = String::from_utf16_lossy(wide);
            // Access 0 is enough to ask what the device is, and works on devices that
            // are opened exclusively by Windows (keyboards, mice).
            let Some(h) = open_handle(&path, 0) else { continue };
            let mut attributes = HiddAttributes {
                size: std::mem::size_of::<HiddAttributes>() as u32,
                vendor_id: 0,
                product_id: 0,
                version: 0,
            };
            if unsafe { HidD_GetAttributes(h.0, &mut attributes) } == 0
                || attributes.vendor_id != VENDOR_ID
                || ![PID_DUALSENSE, PID_DUALSENSE_EDGE].contains(&attributes.product_id)
            {
                continue;
            }
            if let Some(caps) = caps(&h) {
                found.push(Found {
                    path,
                    product_id: attributes.product_id,
                    input_len: caps.input_len as usize,
                });
            }
        }
        found
    }

    fn open(&self, path: &str) -> Option<Box<dyn HidPort>> {
        let handle = open_handle(path, GENERIC_READ | GENERIC_WRITE)?;
        let feature_len = caps(&handle)?.feature_len as usize;
        Some(Box::new(WindowsPort { handle, feature_len }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_zlib() {
        // zlib.crc32(b"\xa2" + b"123456789")
        assert_eq!(crc32(0xA2, b"123456789"), 0x63DA_9F12);
    }

    #[test]
    fn haptics_report_has_the_saxense_layout() {
        let mut pcm = [0u8; HAPTICS_PACKET];
        pcm[0] = 0x7F;
        pcm[63] = 0x81;
        let r = haptics_report(&pcm, 16, 5);
        assert_eq!(&r[..13], &[0x32, 0, 0x91, 0x07, 0xFE, 0, 0, 0, 0, 16, 5, 0x92, 64]);
        assert_eq!((r[13], r[76]), (0x7F, 0x81));
        assert_eq!(u32::from_le_bytes(r[138..].try_into().unwrap()), crc32(0xA2, &r[..138]));
    }

    #[test]
    fn output_report_keeps_the_state_block() {
        let mut usb = [0u8; 48];
        usb[0] = 0x02;
        usb[1] = 0x0C;
        usb[47] = 0x99;
        let r = output_to_bluetooth(&usb, 3);
        assert_eq!(&r[..4], &[0x31, 0x30, 0x10, 0x0C]);
        assert_eq!(r[49], 0x99);
        assert_eq!(u32::from_le_bytes(r[74..].try_into().unwrap()), crc32(0xA2, &r[..74]));
    }

    #[test]
    fn input_report_moves_one_byte_earlier() {
        let mut bt = [0u8; 78];
        bt[0] = 0x31;
        bt[2] = 0x7A;
        bt[64] = 0x55;
        let usb = input_to_usb(&bt);
        assert_eq!((usb[0], usb[1], usb[63]), (0x01, 0x7A, 0x55));
    }

    #[test]
    fn battery_level_and_charging() {
        let mut r = NEUTRAL_INPUT;
        r[53] = 0x08;
        assert_eq!(battery(&r), (80, false));
        r[53] = 0x1A;
        assert_eq!(battery(&r), (100, true));
    }
}
