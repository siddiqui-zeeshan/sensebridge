//! The USB side of a wired DualSense: descriptors and control requests.
//!
//! Everything that needs the real controller goes through a [`Backend`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::descriptors;
use crate::usbip::{DeviceInfo, SPEED_FULL};

/// Where the USB-side traffic ends up.
pub trait Backend: Send + Sync {
    /// Feature report, report ID first, cut to `length` bytes (0 = whole report).
    fn get_feature(&self, report_id: u8, length: usize) -> Option<Vec<u8>>;
    fn set_feature(&self, report_id: u8, data: &[u8]);
    /// USB output report, ID 0x02 first.
    fn output_report(&self, data: &[u8]);
    /// 4-channel signed 16-bit 48 kHz frames; channels 3 and 4 are haptics.
    fn audio_out(&self, pcm: &[u8]);
    fn set_speaker_active(&self, active: bool);
    /// The latest 64-byte USB input report 0x01.
    fn latest_input(&self) -> Vec<u8>;
}

pub const EP_AUDIO_OUT: u32 = 1;
pub const EP_AUDIO_IN: u32 = 2;
pub const EP_HID_OUT: u32 = 3;
pub const EP_HID_IN: u32 = 4;

const ITF_AUDIO_CONTROL: u16 = 0;
const ITF_SPEAKER: u16 = 1;
const ITF_HID: u16 = 3;
const SPEAKER_UNIT: u8 = 0x02;
const MIC_UNIT: u8 = 0x05;

const GET_STATUS: u8 = 0;
const CLEAR_FEATURE: u8 = 1;
const SET_FEATURE: u8 = 3;
const GET_DESCRIPTOR: u8 = 6;
const GET_CONFIGURATION: u8 = 8;
const SET_CONFIGURATION: u8 = 9;
const GET_INTERFACE: u8 = 10;
const SET_INTERFACE: u8 = 11;
const HID_GET_REPORT: u8 = 1;
const HID_GET_IDLE: u8 = 2;
const HID_GET_PROTOCOL: u8 = 3;
const HID_SET_REPORT: u8 = 9;
const HID_SET_IDLE: u8 = 10;
const HID_SET_PROTOCOL: u8 = 11;
const UAC_SET_CUR: u8 = 0x01;
const UAC_GET_CUR: u8 = 0x81;
const UAC_GET_MIN: u8 = 0x82;
const UAC_GET_MAX: u8 = 0x83;
const UAC_GET_RES: u8 = 0x84;
const UAC_MUTE: u8 = 1;
const UAC_VOLUME: u8 = 2;

/// Volume range of an audio feature unit in 1/256 dB, as the dongle firmware reports it.
struct VolumeRange {
    min: i16,
    max: i16,
    res: i16,
}

fn volume_range(unit: u8) -> Option<VolumeRange> {
    match unit {
        SPEAKER_UNIT => Some(VolumeRange { min: -100 * 256, max: 0, res: 256 }),
        MIC_UNIT => Some(VolumeRange { min: 0, max: 48 * 256, res: 0x7A }),
        _ => None,
    }
}

fn string_descriptor(text: &str) -> Vec<u8> {
    let data: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = vec![data.len() as u8 + 2, 3];
    out.extend(data);
    out
}

#[derive(Default)]
struct State {
    config_value: u8,
    alt: HashMap<u16, u8>,
    mute: HashMap<u8, u8>,
    volume: HashMap<u8, i16>,
}

pub struct DualSenseDevice {
    pub backend: Arc<dyn Backend>,
    pub product_id: u16,
    hid_report: &'static [u8],
    configuration: Vec<u8>,
    hid_descriptor_at: usize,
    interfaces: Vec<[u8; 3]>,
    strings: Vec<Vec<u8>>,
    device_descriptor: Vec<u8>,
    state: Mutex<State>,
}

impl DualSenseDevice {
    pub const PATH: &'static str = "/sys/devices/virtual/dualsense/usb1/1-1";
    pub const BUSID: &'static str = "1-1";
    pub const VENDOR_ID: u16 = 0x054C;
    const BCD_DEVICE: u16 = 0x0100;

    pub fn new(backend: Arc<dyn Backend>, edge: bool) -> Self {
        let product_id: u16 = if edge { 0x0DF2 } else { 0x0CE6 };
        let hid_report: &'static [u8] = if edge {
            &descriptors::HID_REPORT_DUALSENSE_EDGE
        } else {
            &descriptors::HID_REPORT_DUALSENSE
        };
        let mut configuration = descriptors::CONFIGURATION.to_vec();
        let mut hid_descriptor_at = 0;
        let mut interfaces: Vec<(u8, [u8; 3])> = Vec::new();
        let mut i = 0;
        while i + 2 <= configuration.len() && configuration[i] != 0 {
            match configuration[i + 1] {
                0x21 if hid_descriptor_at == 0 => hid_descriptor_at = i,
                4 => {
                    let number = configuration[i + 2];
                    if !interfaces.iter().any(|(n, _)| *n == number) {
                        interfaces
                            .push((number, configuration[i + 5..i + 8].try_into().unwrap()));
                    }
                }
                _ => {}
            }
            i += configuration[i] as usize;
        }
        // The HID descriptor carries the report descriptor's length, which differs
        // between the two controller models.
        configuration[hid_descriptor_at + 7..hid_descriptor_at + 9]
            .copy_from_slice(&(hid_report.len() as u16).to_le_bytes());
        interfaces.sort_by_key(|(n, _)| *n);

        let mut device_descriptor = vec![18, 1];
        device_descriptor.extend_from_slice(&0x0200u16.to_le_bytes());
        device_descriptor.extend_from_slice(&[0, 0, 0, 64]);
        device_descriptor.extend_from_slice(&Self::VENDOR_ID.to_le_bytes());
        device_descriptor.extend_from_slice(&product_id.to_le_bytes());
        device_descriptor.extend_from_slice(&Self::BCD_DEVICE.to_le_bytes());
        device_descriptor.extend_from_slice(&[1, 2, 0, 1]);

        let mut state = State::default();
        state.volume.insert(SPEAKER_UNIT, 0);
        state.volume.insert(MIC_UNIT, 48 * 256);

        Self {
            backend,
            product_id,
            hid_report,
            configuration,
            hid_descriptor_at,
            interfaces: interfaces.into_iter().map(|(_, class)| class).collect(),
            strings: vec![
                vec![4, 3, 9, 4],
                string_descriptor("Sony Interactive Entertainment"),
                string_descriptor(if edge {
                    "DualSense Edge Wireless Controller"
                } else {
                    "DualSense Wireless Controller"
                }),
            ],
            device_descriptor,
            state: Mutex::new(state),
        }
    }

    pub fn info(&self) -> DeviceInfo<'_> {
        DeviceInfo {
            path: Self::PATH,
            busid: Self::BUSID,
            busnum: 1,
            devnum: 2,
            speed: SPEED_FULL,
            vendor_id: Self::VENDOR_ID,
            product_id: self.product_id,
            bcd_device: Self::BCD_DEVICE,
            device_class: [0, 0, 0],
            interfaces: &self.interfaces,
        }
    }

    /// Handle one control transfer. `None` stalls it.
    pub fn control(&self, setup: &[u8; 8], data: &[u8]) -> Option<Vec<u8>> {
        let request_type = setup[0];
        let request = setup[1];
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let index = u16::from_le_bytes([setup[4], setup[5]]);
        let length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        let kind = request_type & 0x60;
        let recipient = request_type & 0x1F;

        let mut reply = if kind == 0x00 {
            self.standard(recipient, request, value, index)
        } else if kind == 0x20 && recipient == 1 && index & 0xFF == ITF_HID {
            self.hid(request, value, length, data)
        } else if kind == 0x20 && recipient == 1 && index & 0xFF == ITF_AUDIO_CONTROL {
            self.audio_unit(request, value, (index >> 8) as u8, data)
        } else if kind == 0x20 && recipient == 2 {
            Self::audio_endpoint(request)
        } else {
            None
        }?;
        if request_type & 0x80 != 0 {
            reply.truncate(length);
        }
        Some(reply)
    }

    fn standard(&self, recipient: u8, request: u8, value: u16, index: u16) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        match request {
            GET_DESCRIPTOR => {
                let (kind, number) = ((value >> 8) as u8, (value & 0xFF) as usize);
                match (recipient, kind) {
                    (0, 1) => Some(self.device_descriptor.clone()),
                    (0, 2) => Some(self.configuration.clone()),
                    (0, 3) => self.strings.get(number).cloned(),
                    (1, 0x22) if index == ITF_HID => Some(self.hid_report.to_vec()),
                    (1, 0x21) if index == ITF_HID => Some(
                        self.configuration[self.hid_descriptor_at..self.hid_descriptor_at + 9]
                            .to_vec(),
                    ),
                    _ => None,
                }
            }
            SET_CONFIGURATION => {
                state.config_value = value as u8;
                Some(Vec::new())
            }
            GET_CONFIGURATION => Some(vec![state.config_value]),
            SET_INTERFACE => {
                state.alt.insert(index, value as u8);
                drop(state);
                if index == ITF_SPEAKER {
                    self.backend.set_speaker_active(value != 0);
                }
                Some(Vec::new())
            }
            GET_INTERFACE => Some(vec![state.alt.get(&index).copied().unwrap_or(0)]),
            GET_STATUS => Some(if recipient == 0 { vec![1, 0] } else { vec![0, 0] }),
            CLEAR_FEATURE | SET_FEATURE => Some(Vec::new()),
            _ => None,
        }
    }

    fn hid(&self, request: u8, value: u16, length: usize, data: &[u8]) -> Option<Vec<u8>> {
        let (kind, report_id) = ((value >> 8) as u8, (value & 0xFF) as u8);
        match request {
            HID_GET_REPORT => match kind {
                3 => self.backend.get_feature(report_id, length),
                1 => Some(self.backend.latest_input()),
                _ => None,
            },
            HID_SET_REPORT => {
                match kind {
                    3 => self.backend.set_feature(report_id, data),
                    2 => self.backend.output_report(data),
                    _ => {}
                }
                Some(Vec::new())
            }
            HID_SET_IDLE | HID_SET_PROTOCOL => Some(Vec::new()),
            HID_GET_IDLE => Some(vec![0]),
            HID_GET_PROTOCOL => Some(vec![1]),
            _ => None,
        }
    }

    fn audio_unit(&self, request: u8, value: u16, unit: u8, data: &[u8]) -> Option<Vec<u8>> {
        let range = volume_range(unit)?;
        let mut state = self.state.lock().unwrap();
        match (value >> 8) as u8 {
            UAC_MUTE => match request {
                UAC_SET_CUR if data.len() == 1 => {
                    state.mute.insert(unit, data[0]);
                    Some(Vec::new())
                }
                UAC_GET_CUR => Some(vec![state.mute.get(&unit).copied().unwrap_or(0)]),
                _ => None,
            },
            UAC_VOLUME => match request {
                UAC_SET_CUR if data.len() == 2 => {
                    state.volume.insert(unit, i16::from_le_bytes([data[0], data[1]]));
                    Some(Vec::new())
                }
                UAC_GET_CUR => {
                    Some(state.volume.get(&unit).copied().unwrap_or(0).to_le_bytes().to_vec())
                }
                UAC_GET_MIN => Some(range.min.to_le_bytes().to_vec()),
                UAC_GET_MAX => Some(range.max.to_le_bytes().to_vec()),
                UAC_GET_RES => Some(range.res.to_le_bytes().to_vec()),
                _ => None,
            },
            _ => None,
        }
    }

    fn audio_endpoint(request: u8) -> Option<Vec<u8>> {
        match request {
            UAC_SET_CUR => Some(Vec::new()),
            // Sampling frequency, 3 bytes little-endian.
            UAC_GET_CUR | UAC_GET_MIN | UAC_GET_MAX => Some(48000u32.to_le_bytes()[..3].to_vec()),
            _ => None,
        }
    }
}
