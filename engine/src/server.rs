//! USB/IP server for the one emulated device.
//!
//! Isochronous transfers are completed on a 1 ms-per-packet schedule, standing in for
//! the frame clock a real host controller provides. Without that the host's audio
//! driver would push and pull samples as fast as the socket allows.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::device::{DualSenseDevice, EP_AUDIO_IN, EP_AUDIO_OUT, EP_HID_IN, EP_HID_OUT};
use crate::usbip::{self, IsoResult, Submit};

const FRAME: Duration = Duration::from_millis(1);
/// Largest transfer a client may ask for. The biggest real one is a few kilobytes of
/// audio; anything near this is a broken or hostile client.
const MAX_TRANSFER: usize = 64 * 1024;
/// Transfers one client may have waiting at once. A real host keeps a handful.
const MAX_WAITING: usize = 1024;
/// How long a new connection has to say what it wants.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a reply may sit unsent before the client is taken for dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub type Log = Arc<dyn Fn(String) + Send + Sync>;

/// Transfer counts for one attachment.
#[derive(Default)]
pub struct Stats {
    pub control: AtomicU64,
    pub hid_in: AtomicU64,
    pub hid_out: AtomicU64,
    pub iso_out: AtomicU64,
    pub iso_in: AtomicU64,
    pub stall: AtomicU64,
    pub unlink: AtomicU64,
}

#[derive(Default)]
struct InputState {
    /// Interrupt IN transfers waiting for a report, oldest first.
    pending: VecDeque<u32>,
    /// A report that arrived with nothing waiting for it.
    fresh: Option<Vec<u8>>,
}

struct Scheduled {
    due: Instant,
    order: u64,
    seqnum: u32,
    reply: Vec<u8>,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        (self.due, self.order) == (other.due, other.order)
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.due, self.order).cmp(&(other.due, other.order))
    }
}

#[derive(Default)]
struct Schedule {
    queue: BinaryHeap<Reverse<Scheduled>>,
    cancelled: HashSet<u32>,
    /// When each endpoint's next packet is due to start.
    next_frame: [Option<Instant>; 16],
    order: u64,
}

/// One attached client: transfer dispatch for the lifetime of the connection.
pub struct Session {
    writer: Mutex<TcpStream>,
    /// The same socket, for shutting down while a write holds `writer`.
    control: TcpStream,
    device: Arc<DualSenseDevice>,
    input: Mutex<InputState>,
    schedule: Mutex<Schedule>,
    wake: Condvar,
    alive: AtomicBool,
    started: Instant,
    pub stats: Stats,
}

fn refused(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn read_exact(conn: &mut TcpStream, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    conn.read_exact(&mut buf)?;
    Ok(buf)
}

impl Session {
    fn new(conn: &TcpStream, device: Arc<DualSenseDevice>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            writer: Mutex::new(conn.try_clone()?),
            control: conn.try_clone()?,
            device,
            input: Mutex::default(),
            schedule: Mutex::default(),
            wake: Condvar::new(),
            alive: AtomicBool::new(true),
            started: Instant::now(),
            stats: Stats::default(),
        }))
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn send(&self, data: &[u8]) {
        if self.writer.lock().unwrap().write_all(data).is_err() {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    /// Called from the backend when the controller sends an input report.
    pub fn push_input(&self, report: &[u8]) {
        let seqnum = {
            let mut input = self.input.lock().unwrap();
            let seqnum = input.pending.pop_front();
            input.fresh = if seqnum.is_some() { None } else { Some(report.to_vec()) };
            seqnum
        };
        if let Some(seqnum) = seqnum {
            self.send(&usbip::ret_submit(seqnum, usbip::ST_OK, report, None, 0, &[]));
        }
    }

    fn close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.control.shutdown(Shutdown::Both);
        // Under the lock, so the pacer cannot miss this between its check and its wait.
        let _schedule = self.schedule.lock().unwrap();
        self.wake.notify_all();
    }

    fn run(self: &Arc<Self>, mut conn: TcpStream) {
        let pacer = {
            let session = Arc::clone(self);
            thread::spawn(move || session.pace())
        };
        while self.alive() {
            let Ok(header) = read_exact(&mut conn, usbip::HEADER_SIZE) else { break };
            let header: [u8; usbip::HEADER_SIZE] = header.try_into().unwrap();
            let ok = match usbip::command(&header) {
                usbip::CMD_SUBMIT => self.submit(&mut conn, &header).is_ok(),
                usbip::CMD_UNLINK => {
                    self.unlink(&header);
                    true
                }
                _ => false,
            };
            if !ok {
                break;
            }
        }
        self.close();
        let _ = pacer.join();
        self.device.backend.set_speaker_active(false);
    }

    fn stall(&self, seqnum: u32) {
        self.stats.stall.fetch_add(1, Ordering::Relaxed);
        self.send(&usbip::ret_submit(seqnum, usbip::ST_STALL, &[], None, 0, &[]));
    }

    fn submit(&self, conn: &mut TcpStream, header: &[u8; usbip::HEADER_SIZE]) -> io::Result<()> {
        let urb = Submit::parse(header);
        let length = usize::try_from(urb.length).ok().filter(|&n| n <= MAX_TRANSFER);
        let Some(length) = length else { return Err(refused("transfer size")) };
        if urb.number_of_packets > usbip::MAX_ISO_PACKETS {
            return Err(refused("packet count"));
        }
        let data = if urb.direction == usbip::DIR_OUT && length > 0 {
            read_exact(conn, length)?
        } else {
            Vec::new()
        };
        let packets: Vec<(usize, usize)> = if urb.iso_count() > 0 {
            read_exact(conn, urb.iso_count() * usbip::ISO_PACKET_SIZE)?
                .chunks_exact(usbip::ISO_PACKET_SIZE)
                .map(usbip::parse_iso_packet)
                .collect()
        } else {
            Vec::new()
        };
        // Every packet must lie inside the transfer it belongs to.
        if packets.iter().any(|&(offset, size)| offset.saturating_add(size) > length)
            || packets.iter().map(|p| p.1).sum::<usize>() > length
        {
            return Err(refused("packet bounds"));
        }
        let done: Vec<IsoResult> = packets
            .iter()
            .map(|&(offset, length)| IsoResult {
                offset: offset as u32,
                length: length as u32,
                actual_length: length as u32,
                status: 0,
            })
            .collect();
        let (out, incoming) = (urb.direction == usbip::DIR_OUT, urb.direction == usbip::DIR_IN);

        if urb.ep == 0 {
            self.stats.control.fetch_add(1, Ordering::Relaxed);
            match self.device.control(&urb.setup, &data) {
                None => self.stall(urb.seqnum),
                Some(reply) if incoming => {
                    self.send(&usbip::ret_submit(urb.seqnum, usbip::ST_OK, &reply, None, 0, &[]))
                }
                Some(_) => self.send(&usbip::ret_submit(
                    urb.seqnum,
                    usbip::ST_OK,
                    &[],
                    Some(length as u32),
                    0,
                    &[],
                )),
            }
        } else if urb.ep == EP_HID_IN && incoming {
            self.stats.hid_in.fetch_add(1, Ordering::Relaxed);
            let report = {
                let mut input = self.input.lock().unwrap();
                let report = input.fresh.take();
                if report.is_none() {
                    if input.pending.len() >= MAX_WAITING {
                        return Err(refused("too many waiting transfers"));
                    }
                    input.pending.push_back(urb.seqnum);
                }
                report
            };
            if let Some(report) = report {
                self.send(&usbip::ret_submit(urb.seqnum, usbip::ST_OK, &report, None, 0, &[]));
            }
        } else if urb.ep == EP_HID_OUT && out {
            self.stats.hid_out.fetch_add(1, Ordering::Relaxed);
            self.device.backend.output_report(&data);
            self.send(&usbip::ret_submit(
                urb.seqnum,
                usbip::ST_OK,
                &[],
                Some(length as u32),
                0,
                &[],
            ));
        } else if urb.ep == EP_AUDIO_OUT && out && !packets.is_empty() {
            self.stats.iso_out.fetch_add(1, Ordering::Relaxed);
            // Forward at once; only the completion is held back to frame time.
            let mut pcm = Vec::with_capacity(data.len());
            for &(offset, length) in &packets {
                if let Some(chunk) = data.get(offset..offset + length) {
                    pcm.extend_from_slice(chunk);
                }
            }
            self.device.backend.audio_out(&pcm);
            self.schedule(&urb, packets.len(), |frame| {
                usbip::ret_submit(urb.seqnum, usbip::ST_OK, &[], Some(length as u32), frame, &done)
            })?;
        } else if urb.ep == EP_AUDIO_IN && incoming && !packets.is_empty() {
            self.stats.iso_in.fetch_add(1, Ordering::Relaxed);
            // The microphone is not implemented: answer with silence.
            let silence = vec![0u8; packets.iter().map(|p| p.1).sum()];
            self.schedule(&urb, packets.len(), |frame| {
                usbip::ret_submit(urb.seqnum, usbip::ST_OK, &silence, None, frame, &done)
            })?;
        } else {
            self.stall(urb.seqnum);
        }
        Ok(())
    }

    fn schedule(
        &self,
        urb: &Submit,
        packets: usize,
        make_reply: impl FnOnce(i32) -> Vec<u8>,
    ) -> io::Result<()> {
        let now = Instant::now();
        let mut schedule = self.schedule.lock().unwrap();
        if schedule.queue.len() >= MAX_WAITING {
            return Err(refused("too many waiting transfers"));
        }
        let slot = &mut schedule.next_frame[(urb.ep & 0x0F) as usize];
        let start = slot.map_or(now, |next| next.max(now));
        let due = start + FRAME * packets as u32;
        *slot = Some(due);
        let frame = ((start - self.started).as_millis() & 0x7FFF_FFFF) as i32;
        schedule.order += 1;
        let order = schedule.order;
        schedule.queue.push(Reverse(Scheduled {
            due,
            order,
            seqnum: urb.seqnum,
            reply: make_reply(frame),
        }));
        drop(schedule);
        self.wake.notify_one();
        Ok(())
    }

    fn pace(&self) {
        while self.alive() {
            let mut schedule = self.schedule.lock().unwrap();
            while self.alive() && schedule.queue.is_empty() {
                schedule = self.wake.wait(schedule).unwrap();
            }
            let Some(Reverse(next)) = schedule.queue.peek() else { return };
            let delay = next.due.saturating_duration_since(Instant::now());
            if delay.is_zero() {
                let Reverse(item) = schedule.queue.pop().unwrap();
                let cancelled = schedule.cancelled.remove(&item.seqnum);
                drop(schedule);
                if !cancelled {
                    self.send(&item.reply);
                }
            } else {
                drop(schedule);
                // Sleep in frame-sized steps so a newly scheduled earlier completion
                // is never late by more than one frame.
                thread::sleep(delay.min(FRAME));
            }
        }
    }

    fn unlink(&self, header: &[u8; usbip::HEADER_SIZE]) {
        let target = usbip::unlink_target(header);
        self.stats.unlink.fetch_add(1, Ordering::Relaxed);
        let mut found = {
            let mut input = self.input.lock().unwrap();
            let before = input.pending.len();
            input.pending.retain(|&s| s != target);
            input.pending.len() != before
        };
        if !found {
            let mut schedule = self.schedule.lock().unwrap();
            if schedule.queue.iter().any(|Reverse(item)| item.seqnum == target) {
                schedule.cancelled.insert(target);
                found = true;
            }
        }
        let status = if found { usbip::ST_UNLINKED } else { usbip::ST_OK };
        self.send(&usbip::ret_unlink(usbip::header_seqnum(header), status));
    }
}

pub struct Server {
    device: Arc<DualSenseDevice>,
    address: SocketAddr,
    /// Held here until `serve` hands it to the accepting thread.
    listener: Mutex<Option<TcpListener>>,
    acceptor: Mutex<Option<JoinHandle<()>>>,
    session: Mutex<Option<Arc<Session>>>,
    stopping: AtomicBool,
    log: Log,
}

impl Server {
    pub fn bind(device: Arc<DualSenseDevice>, port: u16, log: Log) -> io::Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        Ok(Arc::new(Self {
            device,
            address: listener.local_addr()?,
            listener: Mutex::new(Some(listener)),
            acceptor: Mutex::new(None),
            session: Mutex::new(None),
            stopping: AtomicBool::new(false),
            log,
        }))
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The attached session, if one is alive.
    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone().filter(|s| s.alive())
    }

    pub fn push_input(&self, report: &[u8]) {
        if let Some(session) = self.session() {
            session.push_input(report);
        }
    }

    /// Accept connections on a background thread until [`Server::close`].
    pub fn serve(self: &Arc<Self>) {
        let Some(listener) = self.listener.lock().unwrap().take() else { return };
        let server = Arc::clone(self);
        // The thread owns the listener, so the port is free the moment it exits.
        let acceptor = thread::spawn(move || {
            while !server.stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, peer)) => {
                        let server = Arc::clone(&server);
                        thread::spawn(move || {
                            let _ = server.handle(conn, peer);
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => return,
                }
            }
        });
        *self.acceptor.lock().unwrap() = Some(acceptor);
    }

    /// Stop serving. The port is free again when this returns.
    pub fn close(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(session) = self.session.lock().unwrap().take() {
            session.close();
        }
        self.listener.lock().unwrap().take();
        if let Some(acceptor) = self.acceptor.lock().unwrap().take() {
            let _ = acceptor.join();
        }
    }

    fn handle(&self, mut conn: TcpStream, peer: SocketAddr) -> io::Result<()> {
        conn.set_nonblocking(false)?;
        conn.set_nodelay(true)?;
        conn.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        conn.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let common = read_exact(&mut conn, usbip::OP_COMMON_SIZE)?;
        let code = u16::from_be_bytes([common[2], common[3]]);
        match code {
            usbip::OP_REQ_DEVLIST => conn.write_all(&usbip::devlist_reply(&[self.device.info()])),
            usbip::OP_REQ_IMPORT => {
                let raw = read_exact(&mut conn, usbip::BUSID_SIZE)?;
                let busid = raw.split(|&b| b == 0).next().unwrap_or(&[]);
                // Check and claim in one step, or two clients asking together both win.
                let session = {
                    let mut current = self.session.lock().unwrap();
                    let free = !current.as_ref().is_some_and(|s| s.alive());
                    if busid == DualSenseDevice::BUSID.as_bytes()
                        && free
                        && !self.stopping.load(Ordering::Relaxed)
                    {
                        let session = Session::new(&conn, Arc::clone(&self.device))?;
                        *current = Some(Arc::clone(&session));
                        Some(session)
                    } else {
                        None
                    }
                };
                let Some(session) = session else {
                    return conn.write_all(&usbip::import_reply(None));
                };
                if let Err(error) = conn.write_all(&usbip::import_reply(Some(&self.device.info())))
                {
                    session.close();
                    return Err(error);
                }
                // An attached host may be quiet for as long as it likes.
                conn.set_read_timeout(None)?;
                (self.log)(format!("attached from {peer}"));
                session.run(conn);
                (self.log)("detached".to_string());
                Ok(())
            }
            other => {
                (self.log)(format!("unknown request {other:#06x}"));
                Ok(())
            }
        }
    }
}
