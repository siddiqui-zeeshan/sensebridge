//! USB/IP wire format (protocol 1.1.1). All integers are big-endian.
//!
//! Reference: <https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html>

pub const VERSION: u16 = 0x0111;
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;
pub const CMD_SUBMIT: u32 = 1;
pub const CMD_UNLINK: u32 = 2;
pub const RET_SUBMIT: u32 = 3;
pub const RET_UNLINK: u32 = 4;
pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;
pub const SPEED_FULL: u32 = 2;

pub const ST_OK: i32 = 0;
pub const ST_STALL: i32 = -32; // -EPIPE
pub const ST_UNLINKED: i32 = -104; // -ECONNRESET

pub const OP_COMMON_SIZE: usize = 8;
pub const HEADER_SIZE: usize = 48;
pub const BUSID_SIZE: usize = 32;
pub const ISO_PACKET_SIZE: usize = 16;
pub const MAX_ISO_PACKETS: i32 = 1024;

/// What the device list and import replies say about a device.
pub struct DeviceInfo<'a> {
    pub path: &'a str,
    pub busid: &'a str,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub device_class: [u8; 3],
    /// (class, subclass, protocol) per interface.
    pub interfaces: &'a [[u8; 3]],
}

fn padded(text: &str, size: usize) -> Vec<u8> {
    let mut out = text.as_bytes().to_vec();
    out.resize(size, 0);
    out
}

fn op_common(code: u16, status: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(OP_COMMON_SIZE);
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
    out
}

fn device_record(dev: &DeviceInfo) -> Vec<u8> {
    let mut out = padded(dev.path, 256);
    out.extend(padded(dev.busid, BUSID_SIZE));
    for v in [dev.busnum, dev.devnum, dev.speed] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    for v in [dev.vendor_id, dev.product_id, dev.bcd_device] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&dev.device_class);
    // bConfigurationValue, bNumConfigurations, bNumInterfaces
    out.extend_from_slice(&[1, 1, dev.interfaces.len() as u8]);
    out
}

pub fn devlist_reply(devices: &[DeviceInfo]) -> Vec<u8> {
    let mut out = op_common(OP_REP_DEVLIST, 0);
    out.extend_from_slice(&(devices.len() as u32).to_be_bytes());
    for dev in devices {
        out.extend(device_record(dev));
        for itf in dev.interfaces {
            out.extend_from_slice(&[itf[0], itf[1], itf[2], 0]);
        }
    }
    out
}

/// `None` refuses the import.
pub fn import_reply(dev: Option<&DeviceInfo>) -> Vec<u8> {
    match dev {
        None => op_common(OP_REP_IMPORT, 1),
        Some(dev) => {
            let mut out = op_common(OP_REP_IMPORT, 0);
            out.extend(device_record(dev));
            out
        }
    }
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be_i32(bytes: &[u8], at: usize) -> i32 {
    be_u32(bytes, at) as i32
}

/// One parsed CMD_SUBMIT header. Payload and packet descriptors follow it on the wire.
#[derive(Debug, Clone)]
pub struct Submit {
    pub seqnum: u32,
    pub direction: u32,
    pub ep: u32,
    pub length: i32,
    pub number_of_packets: i32,
    pub setup: [u8; 8],
}

impl Submit {
    pub fn parse(header: &[u8; HEADER_SIZE]) -> Self {
        Self {
            seqnum: be_u32(header, 4),
            direction: be_u32(header, 12),
            ep: be_u32(header, 16),
            length: be_i32(header, 24),
            number_of_packets: be_i32(header, 32),
            setup: header[40..48].try_into().unwrap(),
        }
    }

    /// Number of isochronous packet descriptors that follow, 0 for other transfers.
    pub fn iso_count(&self) -> usize {
        if self.number_of_packets > 0 && self.number_of_packets <= MAX_ISO_PACKETS {
            self.number_of_packets as usize
        } else {
            0
        }
    }
}

pub fn command(header: &[u8; HEADER_SIZE]) -> u32 {
    be_u32(header, 0)
}

pub fn header_seqnum(header: &[u8; HEADER_SIZE]) -> u32 {
    be_u32(header, 4)
}

/// The seqnum a CMD_UNLINK asks to cancel.
pub fn unlink_target(header: &[u8; HEADER_SIZE]) -> u32 {
    be_u32(header, 20)
}

/// (offset, length) of one isochronous packet descriptor.
pub fn parse_iso_packet(raw: &[u8]) -> (usize, usize) {
    (be_u32(raw, 0) as usize, be_u32(raw, 4) as usize)
}

/// One isochronous packet in a reply.
#[derive(Debug, Clone, Copy)]
pub struct IsoResult {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: u32,
}

pub fn ret_submit(
    seqnum: u32,
    status: i32,
    data: &[u8],
    actual_length: Option<u32>,
    start_frame: i32,
    packets: &[IsoResult],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + data.len() + packets.len() * ISO_PACKET_SIZE);
    for v in [RET_SUBMIT, seqnum, 0, 0, 0] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&actual_length.unwrap_or(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&start_frame.to_be_bytes());
    out.extend_from_slice(&(packets.len() as u32).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // error_count
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(data);
    for p in packets {
        for v in [p.offset, p.length, p.actual_length, p.status] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
    out
}

pub fn ret_unlink(seqnum: u32, status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE);
    for v in [RET_UNLINK, seqnum, 0, 0, 0] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&status.to_be_bytes());
    out.resize(HEADER_SIZE, 0);
    out
}
