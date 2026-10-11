//! The emulated DualSense seen from a USB/IP client. No controller or driver needed.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sensebridge_engine::descriptors;
use sensebridge_engine::device::{Backend, DualSenseDevice};
use sensebridge_engine::server::Server;
use sensebridge_engine::usbip::{self, DIR_IN, DIR_OUT};

#[derive(Default)]
struct FakeBackend {
    outputs: Mutex<Vec<Vec<u8>>>,
    audio: Mutex<Vec<Vec<u8>>>,
    speaker: Mutex<Vec<bool>>,
}

impl Backend for FakeBackend {
    fn get_feature(&self, report_id: u8, length: usize) -> Option<Vec<u8>> {
        (report_id == 0x05).then(|| {
            let mut report = vec![0x05];
            report.extend(0..40u8);
            report.truncate(length);
            report
        })
    }
    fn set_feature(&self, _report_id: u8, _data: &[u8]) {}
    fn output_report(&self, data: &[u8]) {
        self.outputs.lock().unwrap().push(data.to_vec());
    }
    fn audio_out(&self, pcm: &[u8]) {
        self.audio.lock().unwrap().push(pcm.to_vec());
    }
    fn set_speaker_active(&self, active: bool) {
        self.speaker.lock().unwrap().push(active);
    }
    fn latest_input(&self) -> Vec<u8> {
        let mut report = vec![0u8; 64];
        report[0] = 1;
        report
    }
}

struct Reply {
    command: u32,
    seq: u32,
    status: i32,
    actual: u32,
    frame: i32,
    data: Vec<u8>,
    packets: Vec<[u32; 4]>,
}

struct Rig {
    backend: Arc<FakeBackend>,
    server: Arc<Server>,
    sock: Option<TcpStream>,
    seq: u32,
    direction: HashMap<u32, u32>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.close();
    }
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

impl Rig {
    fn new() -> Self {
        let backend = Arc::new(FakeBackend::default());
        let device = Arc::new(DualSenseDevice::new(backend.clone(), false));
        let server = Server::bind(device, 0, Arc::new(|_| {})).unwrap();
        server.serve();
        Self { backend, server, sock: None, seq: 0, direction: HashMap::new() }
    }

    fn connect(&mut self) {
        let sock = TcpStream::connect(self.server.address()).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        self.sock = Some(sock);
    }

    fn send(&mut self, data: &[u8]) {
        self.sock.as_mut().unwrap().write_all(data).unwrap();
    }

    fn recv(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.sock.as_mut().unwrap().read_exact(&mut buf).unwrap();
        buf
    }

    fn op(&mut self, code: u16) {
        let mut msg = usbip::VERSION.to_be_bytes().to_vec();
        msg.extend(code.to_be_bytes());
        msg.extend(0u32.to_be_bytes());
        self.send(&msg);
    }

    /// Import the device; returns the reply's status.
    fn import(&mut self, busid: &str) -> u32 {
        self.connect();
        self.op(usbip::OP_REQ_IMPORT);
        let mut name = busid.as_bytes().to_vec();
        name.resize(usbip::BUSID_SIZE, 0);
        self.send(&name);
        let status = be32(&self.recv(8), 4);
        if status == 0 {
            self.recv(312);
        }
        status
    }

    fn attach(&mut self) {
        assert_eq!(self.import("1-1"), 0);
    }

    fn submit(
        &mut self,
        direction: u32,
        ep: u32,
        setup: [u8; 8],
        data: &[u8],
        length: usize,
        packets: &[(u32, u32)],
    ) -> u32 {
        self.seq += 1;
        self.direction.insert(self.seq, direction);
        let mut msg = Vec::new();
        for v in [usbip::CMD_SUBMIT, self.seq, 0x0001_0002, direction, ep, 0, length as u32, 0] {
            msg.extend(v.to_be_bytes());
        }
        let count: i32 = if packets.is_empty() { -1 } else { packets.len() as i32 };
        msg.extend(count.to_be_bytes());
        msg.extend(0u32.to_be_bytes());
        msg.extend(setup);
        if direction == DIR_OUT {
            msg.extend(data);
        }
        for &(offset, length) in packets {
            for v in [offset, length, 0, 0] {
                msg.extend(v.to_be_bytes());
            }
        }
        self.send(&msg);
        self.seq
    }

    fn unlink(&mut self, target: u32) {
        self.seq += 1;
        let mut msg = Vec::new();
        for v in [usbip::CMD_UNLINK, self.seq, 0x0001_0002, 0, 0, target] {
            msg.extend(v.to_be_bytes());
        }
        msg.resize(usbip::HEADER_SIZE, 0);
        self.send(&msg);
    }

    fn reply(&mut self) -> Reply {
        let head = self.recv(usbip::HEADER_SIZE);
        let (command, seq) = (be32(&head, 0), be32(&head, 4));
        let status = be32(&head, 20) as i32;
        if command == usbip::RET_UNLINK {
            return Reply { command, seq, status, actual: 0, frame: 0, data: vec![], packets: vec![] };
        }
        let actual = be32(&head, 24);
        let frame = be32(&head, 28) as i32;
        let count = be32(&head, 32) as usize;
        let data =
            if self.direction[&seq] == DIR_IN { self.recv(actual as usize) } else { Vec::new() };
        let packets = (0..count)
            .map(|_| {
                let raw = self.recv(16);
                [be32(&raw, 0), be32(&raw, 4), be32(&raw, 8), be32(&raw, 12)]
            })
            .collect();
        Reply { command, seq, status, actual, frame, data, packets }
    }

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
        let mut s = [request_type, request, 0, 0, 0, 0, 0, 0];
        s[2..4].copy_from_slice(&value.to_le_bytes());
        s[4..6].copy_from_slice(&index.to_le_bytes());
        s[6..8].copy_from_slice(&length.to_le_bytes());
        s
    }

    fn control_in(&mut self, request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Reply {
        let setup = Self::setup(request_type, request, value, index, length);
        self.submit(DIR_IN, 0, setup, &[], length as usize, &[]);
        self.reply()
    }

    fn control_out(&mut self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8]) -> Reply {
        let setup = Self::setup(request_type, request, value, index, data.len() as u16);
        self.submit(DIR_OUT, 0, setup, data, data.len(), &[]);
        self.reply()
    }
}

#[test]
fn device_list_describes_a_wired_dualsense() {
    let mut rig = Rig::new();
    rig.connect();
    rig.op(usbip::OP_REQ_DEVLIST);
    let head = rig.recv(12);
    let record = rig.recv(312);
    let interfaces = rig.recv(4 * record[311] as usize);
    assert_eq!(&head[..4], &[0x01, 0x11, 0x00, 0x05]);
    assert_eq!(be32(&head, 8), 1);
    assert_eq!(&record[256..260], b"1-1\0");
    assert_eq!(be32(&record, 296), usbip::SPEED_FULL);
    assert_eq!(&record[300..304], &[0x05, 0x4C, 0x0C, 0xE6]);
    let classes: Vec<&[u8]> = interfaces.chunks(4).map(|i| &i[..3]).collect();
    assert_eq!(classes, [[1, 1, 0], [1, 2, 0], [1, 2, 0], [3, 0, 0]]);
}

#[test]
fn unknown_bus_id_is_refused() {
    assert_ne!(Rig::new().import("9-9"), 0);
}

#[test]
fn a_second_client_is_refused_while_one_is_attached() {
    let mut first = Rig::new();
    first.attach();
    first.control_in(0x80, 8, 0, 0, 1);
    let mut second = TcpStream::connect(first.server.address()).unwrap();
    let mut msg = usbip::VERSION.to_be_bytes().to_vec();
    msg.extend(usbip::OP_REQ_IMPORT.to_be_bytes());
    msg.extend(0u32.to_be_bytes());
    msg.extend(b"1-1");
    msg.resize(8 + usbip::BUSID_SIZE, 0);
    second.write_all(&msg).unwrap();
    let mut reply = [0u8; 8];
    second.read_exact(&mut reply).unwrap();
    assert_ne!(be32(&reply, 4), 0);
}

#[test]
fn descriptors_match_a_wired_controller() {
    let mut rig = Rig::new();
    rig.attach();
    let device = rig.control_in(0x80, 6, 0x0100, 0, 18).data;
    assert_eq!(device.len(), 18);
    assert_eq!(&device[8..12], &[0x4C, 0x05, 0xE6, 0x0C]);
    let short = rig.control_in(0x80, 6, 0x0200, 0, 9).data;
    assert_eq!(u16::from_le_bytes([short[2], short[3]]) as usize, descriptors::CONFIGURATION.len());
    let full = rig.control_in(0x80, 6, 0x0200, 0, 1024).data;
    assert_eq!(full.len(), descriptors::CONFIGURATION.len());
    let product = rig.control_in(0x80, 6, 0x0302, 0x0409, 255).data;
    let name: Vec<u16> =
        product[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    assert_eq!(String::from_utf16(&name).unwrap(), "DualSense Wireless Controller");
    let report = rig.control_in(0x81, 6, 0x2200, 3, 1024).data;
    assert_eq!(report, descriptors::HID_REPORT_DUALSENSE);
}

#[test]
fn unsupported_request_stalls() {
    let mut rig = Rig::new();
    rig.attach();
    assert_eq!(rig.control_in(0x80, 6, 0x0600, 0, 10).status, usbip::ST_STALL);
}

#[test]
fn feature_report_comes_from_the_controller() {
    let mut rig = Rig::new();
    rig.attach();
    let got = rig.control_in(0xA1, 1, 0x0305, 3, 41);
    assert_eq!(got.status, 0);
    let mut expected = vec![0x05];
    expected.extend(0..40u8);
    assert_eq!(got.data, expected);
    assert_eq!(rig.control_in(0xA1, 1, 0x03F0, 3, 64).status, usbip::ST_STALL);
}

#[test]
fn speaker_volume_range_and_setting() {
    let mut rig = Rig::new();
    rig.attach();
    assert_eq!(rig.control_in(0xA1, 0x82, 0x0200, 0x0200, 2).data, (-100i16 * 256).to_le_bytes());
    assert_eq!(rig.control_in(0xA1, 0x83, 0x0200, 0x0200, 2).data, 0i16.to_le_bytes());
    assert_eq!(rig.control_out(0x21, 0x01, 0x0200, 0x0200, &[0x00, 0xF6]).status, 0);
    assert_eq!(rig.control_in(0xA1, 0x81, 0x0200, 0x0200, 2).data, [0x00, 0xF6]);
}

#[test]
fn input_report_completes_a_waiting_transfer() {
    let mut rig = Rig::new();
    rig.attach();
    let seq = rig.submit(DIR_IN, 4, [0; 8], &[], 64, &[]);
    thread::sleep(Duration::from_millis(50));
    let mut report = vec![1u8];
    report.extend(0..63u8);
    rig.server.push_input(&report);
    let got = rig.reply();
    assert_eq!((got.seq, got.status, got.data), (seq, 0, report));
}

#[test]
fn input_report_that_arrived_early_is_delivered_at_once() {
    let mut rig = Rig::new();
    rig.attach();
    rig.control_in(0x80, 8, 0, 0, 1); // the session is up once this round trip is done
    let report = [7u8; 64];
    rig.server.push_input(&report);
    rig.submit(DIR_IN, 4, [0; 8], &[], 64, &[]);
    assert_eq!(rig.reply().data, report);
}

#[test]
fn output_report_reaches_the_controller() {
    let mut rig = Rig::new();
    rig.attach();
    let mut report = vec![0x02, 0x0C];
    report.resize(48, 0);
    rig.submit(DIR_OUT, 3, [0; 8], &report, 48, &[]);
    assert_eq!(rig.reply().actual, 48);
    assert_eq!(*rig.backend.outputs.lock().unwrap(), vec![report]);
}

#[test]
fn audio_is_forwarded_at_once_and_completed_at_frame_rate() {
    let mut rig = Rig::new();
    rig.attach();
    rig.control_out(0x01, 11, 1, 1, &[]); // SET_INTERFACE: speaker, alternate setting 1
    assert_eq!(*rig.backend.speaker.lock().unwrap(), vec![true]);
    let packets: Vec<(u32, u32)> = (0..10).map(|i| (i * 384, 384)).collect();
    let audio: Vec<u8> = (0..3840).map(|i| i as u8).collect();
    let start = Instant::now();
    let first = rig.submit(DIR_OUT, 1, [0; 8], &audio, audio.len(), &packets);
    let second = rig.submit(DIR_OUT, 1, [0; 8], &audio, audio.len(), &packets);
    let a = rig.reply();
    let t_a = start.elapsed();
    let b = rig.reply();
    let t_b = start.elapsed();
    assert_eq!(*rig.backend.audio.lock().unwrap(), vec![audio.clone(), audio.clone()]);
    assert_eq!((a.seq, b.seq), (first, second));
    assert_eq!(a.packets, packets.iter().map(|&(o, n)| [o, n, n, 0]).collect::<Vec<_>>());
    assert_eq!(a.actual, 3840);
    assert!(t_a > Duration::from_millis(8), "first completion after {t_a:?}");
    assert!(t_b - t_a > Duration::from_millis(8), "second came {:?} after the first", t_b - t_a);
    assert!(t_b < Duration::from_millis(60), "both took {t_b:?}");
    assert_eq!(b.frame - a.frame, 10);
}

#[test]
fn microphone_returns_silence_at_frame_rate() {
    let mut rig = Rig::new();
    rig.attach();
    let packets: Vec<(u32, u32)> = (0..8).map(|i| (i * 192, 192)).collect();
    let start = Instant::now();
    rig.submit(DIR_IN, 2, [0; 8], &[], 1536, &packets);
    let got = rig.reply();
    assert!(start.elapsed() > Duration::from_millis(6));
    assert_eq!(got.data, vec![0u8; 1536]);
    assert_eq!(got.packets.len(), 8);
}

#[test]
fn unlink_cancels_a_waiting_input_transfer() {
    let mut rig = Rig::new();
    rig.attach();
    let target = rig.submit(DIR_IN, 4, [0; 8], &[], 64, &[]);
    thread::sleep(Duration::from_millis(50));
    rig.unlink(target);
    let got = rig.reply();
    assert_eq!((got.command, got.status), (usbip::RET_UNLINK, usbip::ST_UNLINKED));
    // The cancelled transfer must not be completed by a later report.
    rig.server.push_input(&[0u8; 64]);
    let fresh = rig.submit(DIR_IN, 4, [0; 8], &[], 64, &[]);
    assert_eq!(rig.reply().seq, fresh);
}

#[test]
fn unlink_of_a_finished_transfer_reports_success() {
    let mut rig = Rig::new();
    rig.attach();
    let done = rig.control_in(0x80, 8, 0, 0, 1).seq;
    rig.unlink(done);
    assert_eq!(rig.reply().status, 0);
}

#[test]
fn closing_the_connection_stops_the_speaker_and_frees_the_device() {
    let mut rig = Rig::new();
    rig.attach();
    rig.control_out(0x01, 11, 1, 1, &[]);
    rig.sock = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while rig.backend.speaker.lock().unwrap().last() != Some(&false) {
        assert!(Instant::now() < deadline, "speaker was left on");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(rig.server.session().is_none());
    rig.attach(); // and it can be attached again
}

/// Whether the server ends the connection within `within`.
fn closed_by_server(sock: &mut TcpStream, within: Duration) -> bool {
    sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let deadline = Instant::now() + within;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match sock.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return true,
        }
    }
    false
}

fn import_request() -> Vec<u8> {
    let mut msg = usbip::VERSION.to_be_bytes().to_vec();
    msg.extend(usbip::OP_REQ_IMPORT.to_be_bytes());
    msg.extend(0u32.to_be_bytes());
    msg.extend(b"1-1");
    msg.resize(8 + usbip::BUSID_SIZE, 0);
    msg
}

#[test]
fn a_transfer_claiming_an_absurd_size_ends_the_connection() {
    let mut rig = Rig::new();
    rig.attach();
    rig.submit(DIR_OUT, 3, [0; 8], &[], 0x7FFF_FFFF, &[]);
    assert!(
        closed_by_server(rig.sock.as_mut().unwrap(), Duration::from_secs(2)),
        "the server went on waiting for 2 GiB of payload"
    );
}

#[test]
fn microphone_packets_claiming_an_absurd_size_end_the_connection() {
    let mut rig = Rig::new();
    rig.attach();
    let packets = vec![(0u32, u32::MAX); 1024];
    rig.submit(DIR_IN, 2, [0; 8], &[], 1536, &packets);
    assert!(closed_by_server(rig.sock.as_mut().unwrap(), Duration::from_secs(2)));
}

#[test]
fn too_many_isochronous_packets_end_the_connection() {
    let mut rig = Rig::new();
    rig.attach();
    // By hand: the header claims more packet descriptors than any host sends.
    let mut msg = Vec::new();
    for v in [usbip::CMD_SUBMIT, 1, 0x0001_0002, DIR_IN, 2, 0, 1536, 0, 100_000, 0] {
        msg.extend(v.to_be_bytes());
    }
    msg.extend([0u8; 8]);
    rig.send(&msg);
    assert!(closed_by_server(rig.sock.as_mut().unwrap(), Duration::from_secs(2)));
}

#[test]
fn a_client_that_queues_transfers_without_end_is_dropped() {
    let mut rig = Rig::new();
    rig.attach();
    rig.sock.as_ref().unwrap().set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    // Completions leave at one per millisecond; these arrive far faster.
    for seq in 1..=20_000u32 {
        let mut msg = Vec::new();
        for v in [usbip::CMD_SUBMIT, seq, 0x0001_0002, DIR_IN, 2, 0, 192, 0, 1, 0] {
            msg.extend(v.to_be_bytes());
        }
        msg.extend([0u8; 8]);
        for v in [0u32, 192, 0, 0] {
            msg.extend(v.to_be_bytes());
        }
        if rig.sock.as_mut().unwrap().write_all(&msg).is_err() {
            return; // already dropped
        }
    }
    assert!(closed_by_server(rig.sock.as_mut().unwrap(), Duration::from_secs(2)));
}

#[test]
fn closing_does_not_wait_for_a_client_that_stopped_reading() {
    let mut rig = Rig::new();
    rig.attach();
    rig.sock.as_ref().unwrap().set_write_timeout(Some(Duration::from_millis(500))).unwrap();
    // Ask for the configuration descriptor until the replies nobody reads have filled
    // the socket and the server is stuck writing one.
    let setup = Rig::setup(0x80, 6, 0x0200, 0, 1024);
    let mut msg = Vec::new();
    for v in [usbip::CMD_SUBMIT, 1, 0x0001_0002, DIR_IN, 0, 0, 1024, 0, 0xFFFF_FFFF, 0] {
        msg.extend(v.to_be_bytes());
    }
    msg.extend(setup);
    let mut stuck = false;
    for _ in 0..2_000_000 {
        if rig.sock.as_mut().unwrap().write_all(&msg).is_err() {
            stuck = true;
            break;
        }
    }
    assert!(stuck, "could not fill the socket");

    let server = Arc::clone(&rig.server);
    let (done, finished) = std::sync::mpsc::channel();
    thread::spawn(move || {
        server.close();
        let _ = done.send(());
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(3)).is_ok(),
        "close() is still waiting on the stuck write"
    );
}

#[test]
fn two_clients_importing_at_the_same_moment_do_not_both_get_the_device() {
    for round in 0..30 {
        let rig = Rig::new();
        let address = rig.server.address();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let clients: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let mut sock = TcpStream::connect(address).unwrap();
                    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    barrier.wait();
                    sock.write_all(&import_request()).unwrap();
                    let mut reply = [0u8; 8];
                    sock.read_exact(&mut reply).unwrap();
                    (be32(&reply, 4), sock)
                })
            })
            .collect();
        let results: Vec<_> = clients.into_iter().map(|c| c.join().unwrap()).collect();
        let accepted = results.iter().filter(|(status, _)| *status == 0).count();
        assert_eq!(accepted, 1, "round {round}");
    }
}

#[test]
fn a_connection_that_never_asks_for_anything_is_dropped() {
    let rig = Rig::new();
    let mut idle = TcpStream::connect(rig.server.address()).unwrap();
    assert!(
        closed_by_server(&mut idle, Duration::from_secs(5)),
        "an idle connection was kept open"
    );
}
