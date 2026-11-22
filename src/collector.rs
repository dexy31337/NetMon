//! All unsafe Windows FFI is kept in this module. ETW callbacks never block on storage/UI.
mod wire_capture;
use crate::model::*;
use anyhow::{Context, Result, bail};
use crossbeam_channel::Sender;
use std::{
    mem::{size_of, zeroed},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{
            Diagnostics::{Etw::*, ToolHelp::*},
            Threading::*,
        },
    },
    core::GUID,
};

const TCP: GUID = GUID::from_u128(0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2);
const UDP: GUID = GUID::from_u128(0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80);
const PROCESS: GUID = GUID::from_u128(0x3d6fa8d0_fe05_11d0_9dda_00c04fd7ba7c);
const DNS: GUID = GUID::from_u128(0x1c95126e_7eea_49a9_a3fe_a378b03ddb4d);
const AFD: GUID = GUID::from_u128(0xe53c6823_7bb8_44bb_90dc_3f86090d48a6);

fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn text16(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())])
}
fn filetime_ms(t: u64) -> i64 {
    (t / 10_000) as i64 - 11_644_473_600_000
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut len = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

pub fn process_info(pid: u32) -> Option<ProcessInfo> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        let mut created: FILETIME = zeroed();
        let mut exit: FILETIME = zeroed();
        let mut kernel: FILETIME = zeroed();
        let mut user: FILETIME = zeroed();
        let timed = GetProcessTimes(h, &mut created, &mut exit, &mut kernel, &mut user);
        CloseHandle(h);
        if ok == 0 || timed == 0 {
            return None;
        }
        let path = text16(&buf[..len as usize]);
        let exe = path.rsplit('\\').next().unwrap_or(&path).to_owned();
        Some(ProcessInfo {
            pid,
            parent_pid: 0,
            started: filetime_ms(
                ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
            ),
            exe,
            path,
        })
    }
}

pub fn processes() -> Vec<ProcessInfo> {
    let mut out = Vec::new();
    visit_processes(|entry| {
        let mut p = process_info(entry.th32ProcessID).unwrap_or_else(|| ProcessInfo {
            pid: entry.th32ProcessID,
            exe: text16(&entry.szExeFile),
            ..Default::default()
        });
        p.parent_pid = entry.th32ParentProcessID;
        out.push(p);
    });
    out
}

pub fn executable_names() -> Vec<String> {
    let mut names = std::collections::BTreeMap::new();
    visit_processes(|entry| {
        let name = text16(&entry.szExeFile);
        let key = name.to_lowercase();
        if key.ends_with(".exe") {
            names.entry(key).or_insert(name);
        }
    });
    names.into_values().collect()
}

fn visit_processes(mut visit: impl FnMut(&PROCESSENTRY32W)) {
    unsafe {
        let h = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(h, &mut entry);
        while ok != 0 {
            visit(&entry);
            ok = Process32NextW(h, &mut entry);
        }
        CloseHandle(h);
    }
}

#[derive(Default)]
pub struct Counters {
    pub queue_lost: AtomicU64,
    pub decode_errors: AtomicU64,
    sources: Mutex<std::collections::BTreeMap<String, u64>>,
}
pub struct CapturedEvent {
    pub ticks: i64,
    pub event: Event,
}
struct Callback {
    tx: Sender<CapturedEvent>,
    counters: Arc<Counters>,
    creations: Mutex<std::collections::HashMap<(u32, u32), (u32, u32)>>,
}

#[repr(C)]
struct Properties {
    p: EVENT_TRACE_PROPERTIES,
    name: [u16; 128],
}
fn properties() -> Box<Properties> {
    let mut props = Box::new(Properties {
        p: EVENT_TRACE_PROPERTIES::default(),
        name: [0; 128],
    });
    props.p.Wnode.BufferSize = size_of::<Properties>() as u32;
    props.p.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    props
}

struct Session {
    handle: CONTROLTRACE_HANDLE,
    consumer: PROCESSTRACE_HANDLE,
    thread: Option<JoinHandle<u32>>,
    stopped: bool,
}

impl Session {
    fn start(
        name: &str,
        kernel: bool,
        tx: Sender<CapturedEvent>,
        counters: Arc<Counters>,
    ) -> Result<Self> {
        unsafe {
            let mut p = properties();
            p.p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            p.p.Wnode.ClientContext = 1; // QPC clock, converted by ProcessTrace to FILETIME.
            p.p.Wnode.Guid = GUID::from_u128(
                0x826a7078_1200_4800_9000_000000000000 | ((now_ms() as u128) << 8) | kernel as u128,
            );
            p.p.BufferSize = 64;
            p.p.MinimumBuffers = 64;
            p.p.MaximumBuffers = 256;
            p.p.FlushTimer = 1;
            p.p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE
                | if kernel {
                    EVENT_TRACE_SYSTEM_LOGGER_MODE
                } else {
                    0
                };
            if kernel {
                p.p.EnableFlags = EVENT_TRACE_FLAG_NETWORK_TCPIP | EVENT_TRACE_FLAG_PROCESS;
            }
            let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
            let mut name = wide(name);
            let code = StartTraceW(&mut handle, name.as_ptr(), &mut p.p);
            if code != 0 {
                bail!(
                    "StartTraceW: {} (код {code}). Запустите NetMon от имени администратора.",
                    std::io::Error::from_raw_os_error(code as i32)
                );
            }
            let mut session = Self {
                handle,
                consumer: PROCESSTRACE_HANDLE { Value: u64::MAX },
                thread: None,
                stopped: false,
            };
            if !kernel {
                enable(handle, &DNS, &[3006, 3008]).context("Не удалось включить DNS ETW")?;
                enable(
                    handle,
                    &AFD,
                    &[
                        1, 2, 3, 4, 5, 6, 13, 14, 1000, 1001, 1002, 1017, 1018, 1020, 1021, 1030,
                    ],
                )
                .context("Не удалось включить Winsock-AFD ETW")?;
            }
            let mut callback = Box::new(Callback {
                tx,
                counters,
                creations: Mutex::new(std::collections::HashMap::new()),
            });
            let mut log = EVENT_TRACE_LOGFILEW {
                LoggerName: name.as_mut_ptr(),
                ..Default::default()
            };
            log.Anonymous1.ProcessTraceMode =
                PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
            log.Anonymous2.EventRecordCallback = Some(on_event);
            log.Context = callback.as_mut() as *mut Callback as _;
            session.consumer = OpenTraceW(&mut log);
            if session.consumer.Value == u64::MAX {
                bail!("OpenTraceW: {}", std::io::Error::last_os_error());
            }
            let consumer_value = session.consumer.Value;
            session.thread = Some(thread::spawn(move || {
                let handle = PROCESSTRACE_HANDLE {
                    Value: consumer_value,
                };
                let code = ProcessTrace(&handle, 1, null(), null());
                drop(callback); // Context must outlive every callback.
                code
            }));
            Ok(session)
        }
    }

    fn lost(&self) -> u64 {
        unsafe {
            let mut p = properties();
            if ControlTraceW(self.handle, null(), &mut p.p, EVENT_TRACE_CONTROL_QUERY) != 0 {
                return 0;
            }
            p.p.EventsLost as u64 + p.p.RealTimeBuffersLost as u64 + p.p.LogBuffersLost as u64
        }
    }

    fn stop(&mut self) -> u64 {
        if self.stopped {
            return 0;
        }
        self.stopped = true;
        unsafe {
            let mut p = properties();
            ControlTraceW(self.handle, null(), &mut p.p, EVENT_TRACE_CONTROL_STOP);
            // Stopping the controller flushes buffers; ProcessTrace exits after draining them.
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            if self.consumer.Value != u64::MAX {
                CloseTrace(self.consumer);
            }
            p.p.EventsLost as u64 + p.p.RealTimeBuffersLost as u64 + p.p.LogBuffersLost as u64
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe fn enable(handle: CONTROLTRACE_HANDLE, provider: &GUID, ids: &[u16]) -> Result<()> {
    #[repr(C)]
    struct IdFilter {
        filter_in: u8,
        reserved: u8,
        count: u16,
        ids: [u16; 16],
    }
    let mut filter = IdFilter {
        filter_in: 1,
        reserved: 0,
        count: ids.len() as u16,
        ids: [0; 16],
    };
    filter.ids[..ids.len()].copy_from_slice(ids);
    let descriptor = EVENT_FILTER_DESCRIPTOR {
        Ptr: &filter as *const _ as u64,
        Size: (4 + ids.len() * 2) as u32,
        Type: EVENT_FILTER_TYPE_EVENT_ID,
    };
    let mut params = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableFilterDesc: &descriptor as *const _ as _,
        FilterDescCount: 1,
        ..Default::default()
    };
    let code = unsafe {
        EnableTraceEx2(
            handle,
            provider,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER,
            5,
            u64::MAX,
            0,
            0,
            if ids.is_empty() {
                null_mut()
            } else {
                &mut params
            },
        )
    };
    if code != 0 {
        bail!("EnableTraceEx2: {code}");
    }
    Ok(())
}

pub struct Collector {
    sessions: Vec<Session>,
    wire: wire_capture::Capture,
    pub counters: Arc<Counters>,
}
impl Collector {
    pub fn start(tx: Sender<CapturedEvent>) -> Result<Self> {
        let counters = Arc::new(Counters::default());
        let mut this = Self {
            sessions: Vec::new(),
            wire: wire_capture::Capture::default(),
            counters,
        };
        let suffix = format!("{}-{}", std::process::id(), now_ms());
        this.sessions.push(Session::start(
            &format!("NetMon-Network-{suffix}"),
            true,
            tx.clone(),
            this.counters.clone(),
        )?);
        this.sessions.push(Session::start(
            &format!("NetMon-DNS-AFD-{suffix}"),
            false,
            tx.clone(),
            this.counters.clone(),
        )?);
        this.wire.start(tx, this.counters.clone());
        Ok(this)
    }
    pub fn update_health(&self, h: &mut Health) {
        self.wire.health(h);
        h.queue_lost = self.counters.queue_lost.load(Ordering::Relaxed);
        h.decode_errors = self.counters.decode_errors.load(Ordering::Relaxed);
        h.etw_lost = self.sessions.iter().map(Session::lost).sum();
        h.source_events = self.counters.sources.lock().unwrap().clone();
    }
    pub fn stop(&mut self, h: &mut Health) {
        self.wire.stop();
        self.wire.health(h);
        h.etw_lost = self.sessions.iter_mut().map(Session::stop).sum();
        h.queue_lost = self.counters.queue_lost.load(Ordering::Relaxed);
        h.decode_errors = self.counters.decode_errors.load(Ordering::Relaxed);
        h.source_events = self.counters.sources.lock().unwrap().clone();
    }
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let record = unsafe { &*record };
    if record.UserContext.is_null() {
        return;
    }
    let callback = unsafe { &*(record.UserContext as *const Callback) };
    // No Rust unwind may cross the Windows callback boundary.
    let result = std::panic::catch_unwind(|| {
        let provider = &record.EventHeader.ProviderId;
        if same(provider, &AFD) || same(provider, &DNS) {
            let name = if same(provider, &DNS) { "DNS" } else { "AFD" };
            let key = format!("{name}/{}", record.EventHeader.EventDescriptor.Id);
            *callback
                .counters
                .sources
                .lock()
                .unwrap()
                .entry(key)
                .or_default() += 1;
        }
        decode(record, callback)
    });
    match result {
        Ok(Ok(Some(event))) => {
            if callback
                .tx
                .try_send(CapturedEvent {
                    ticks: record.EventHeader.TimeStamp,
                    event,
                })
                .is_err()
            {
                callback.counters.queue_lost.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Ok(None)) => (),
        _ => {
            callback
                .counters
                .decode_errors
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn property(record: &EVENT_RECORD, name: &str) -> Result<Vec<u8>> {
    let name = wide(name);
    let descriptor = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: name.as_ptr() as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    };
    let mut len = 0;
    unsafe {
        let code = TdhGetPropertySize(record, 0, null(), 1, &descriptor, &mut len);
        if code != 0 || len > 1_048_576 {
            bail!("TDH property size: {code}");
        }
        let mut buf = vec![0; len as usize];
        let code = TdhGetProperty(record, 0, null(), 1, &descriptor, len, buf.as_mut_ptr());
        if code != 0 {
            bail!("TDH property: {code}");
        }
        Ok(buf)
    }
}
fn number(record: &EVENT_RECORD, name: &str) -> Result<u64> {
    let b = property(record, name)?;
    if b.is_empty() || b.len() > 8 {
        bail!("Unexpected numeric property length");
    }
    let mut out = [0; 8];
    out[..b.len()].copy_from_slice(&b);
    Ok(u64::from_le_bytes(out))
}
fn string(record: &EVENT_RECORD, name: &str) -> Result<String> {
    let b = property(record, name)?;
    let chars: Vec<_> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    Ok(text16(&chars))
}
fn afd_addr(record: &EVENT_RECORD) -> Result<SocketAddr> {
    let b = property(record, "Address")?;
    let ip = match b.len() {
        4 => IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])),
        16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(b.as_slice()).unwrap())),
        _ => bail!("Unexpected AFD address length"),
    };
    let port = property(record, "Port")?;
    if port.len() != 2 {
        bail!("Unexpected AFD port length");
    }
    Ok(SocketAddr::new(ip, u16::from_be_bytes([port[0], port[1]])))
}

fn sockaddr(b: &[u8]) -> Option<SocketAddr> {
    if b.len() < 8 {
        return None;
    }
    let port = u16::from_be_bytes([b[2], b[3]]);
    match u16::from_le_bytes([b[0], b[1]]) {
        2 => Some(SocketAddr::new(
            Ipv4Addr::new(b[4], b[5], b[6], b[7]).into(),
            port,
        )),
        23 if b.len() >= 28 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&b[8..24]).ok()?);
            if let Some(ip) = ip.to_ipv4_mapped() {
                return Some(SocketAddr::new(ip.into(), port));
            }
            let scope = u32::from_le_bytes(b[24..28].try_into().ok()?);
            Some(std::net::SocketAddrV6::new(ip, port, 0, scope).into())
        }
        _ => None,
    }
}

fn decode(record: &EVENT_RECORD, callback: &Callback) -> Result<Option<Event>> {
    let header = &record.EventHeader;
    let at = filetime_ms(header.TimeStamp as u64);
    let id = header.EventDescriptor.Id;
    let opcode = header.EventDescriptor.Opcode;
    if same(&header.ProviderId, &TCP) || same(&header.ProviderId, &UDP) {
        let protocol = if same(&header.ProviderId, &TCP) {
            Protocol::Tcp
        } else {
            Protocol::Udp
        };
        let kind = match opcode {
            10 | 26 => NetKind::Send,
            11 | 27 => NetKind::Receive,
            12 | 28 if protocol == Protocol::Tcp => NetKind::Connect,
            13 | 29 if protocol == Protocol::Tcp => NetKind::Disconnect,
            14 | 30 if protocol == Protocol::Tcp => NetKind::Retransmit,
            15 | 31 if protocol == Protocol::Tcp => NetKind::Accept,
            _ => return Ok(None),
        };
        if record.UserData.is_null() {
            bail!("Missing kernel payload");
        }
        let payload = unsafe {
            std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize)
        };
        return decode_network(
            payload,
            header.EventDescriptor.Version,
            opcode >= 26,
            protocol,
            kind,
            at,
        )
        .map(Some);
    }
    if same(&header.ProviderId, &DNS) && matches!(id, 3006 | 3008) {
        let g = &header.ActivityId;
        let activity = ((g.data1 as u128) << 96)
            | ((g.data2 as u128) << 80)
            | ((g.data3 as u128) << 64)
            | u64::from_be_bytes(g.data4) as u128;
        return Ok(Some(Event::Dns {
            at,
            pid: header.ProcessId,
            name: string(record, "QueryName")?,
            query_type: number(record, "QueryType")? as u32,
            status: if id == 3008 {
                number(record, "QueryStatus")? as u32
            } else {
                0
            },
            results: if id == 3008 {
                string(record, "QueryResults")?
            } else {
                String::new()
            },
            activity,
            completed: id == 3008,
        }));
    }
    if same(&header.ProviderId, &AFD) {
        if !matches!(id, 1..=6 | 13 | 14 | 1000..=1002 | 1017 | 1018 | 1020 | 1021 | 1030) {
            return Ok(None);
        }
        let endpoint = number(record, "Endpoint")?;
        if id == 1000 {
            // Entry carries protocol/type; exit carries the allocated endpoint.
            // Correlate on the issuing thread, not on a pointer that is zero at entry.
            let key = (header.ProcessId, header.ThreadId);
            let status = number(record, "Status")? as u32;
            let phase = number(record, "EnterExit")?;
            if phase == 0 {
                let pid = number(record, "ProcessId")? as u32;
                let protocol = match number(record, "Protocol")? as u32 {
                    0 => match number(record, "SocketType")? {
                        1 => 6,
                        2 => 17,
                        _ => 0,
                    },
                    p => p,
                };
                callback
                    .creations
                    .lock()
                    .unwrap()
                    .insert(key, (pid, protocol));
                return Ok(None);
            }
            let pending = callback.creations.lock().unwrap().remove(&key);
            return Ok(pending
                .filter(|_| status == 0 && endpoint != 0)
                .map(|(pid, protocol)| Event::SocketCreate {
                    at,
                    endpoint,
                    pid,
                    protocol,
                }));
        }
        if endpoint == 0 {
            return Ok(None);
        }
        if id >= 1000 {
            let status = number(record, "Status")? as u32;
            return Ok(match id {
                1002 if number(record, "EnterExit")? == 1 => {
                    Some(Event::SocketClose { at, endpoint })
                }
                1018 | 1021 => {
                    sockaddr(&property(record, "Address")?).map(|remote| Event::SocketConnect {
                        at,
                        endpoint,
                        pid: header.ProcessId,
                        remote,
                        status,
                    })
                }
                // Only a failure is conclusive here: an entry event can also have Status=0.
                1017 | 1020 if status & 0x8000_0000 != 0 => Some(Event::SocketResult {
                    at,
                    endpoint,
                    status,
                }),
                1030 if status == 0 => {
                    sockaddr(&property(record, "Address")?).map(|local| Event::SocketBind {
                        at,
                        endpoint,
                        local,
                    })
                }
                _ => None,
            });
        }
        return Ok(match id {
            1 => Some(Event::SocketCreate {
                at,
                endpoint,
                pid: number(record, "UserModePid")? as u32,
                protocol: match number(record, "Protocol")? as u32 {
                    0 => match number(record, "SocketType")? {
                        1 => 6,
                        2 => 17,
                        _ => 0,
                    },
                    p => p,
                },
            }),
            2 | 3 if number(record, "Status")? == 0 => Some(Event::SocketBind {
                at,
                endpoint,
                local: afd_addr(record)?,
            }),
            4 | 5 => Some(Event::SocketConnect {
                at,
                endpoint,
                pid: header.ProcessId,
                remote: afd_addr(record)?,
                status: 0,
            }),
            6 => Some(Event::SocketResult {
                at,
                endpoint,
                status: number(record, "Error")? as u32,
            }),
            13 | 14 => Some(Event::SocketClose { at, endpoint }),
            _ => None,
        });
    }
    if same(&header.ProviderId, &PROCESS) && matches!(opcode, 1..=4) {
        let pid = number(record, "ProcessId")? as u32;
        if opcode == 2 {
            return Ok(Some(Event::ProcessEnd { at, pid }));
        }
        if opcode == 4 {
            return Ok(None);
        }
        let mut p = process_info(pid)
            .filter(|p| p.started <= at)
            .unwrap_or_else(|| {
                let bytes = property(record, "ImageFileName").unwrap_or_default();
                let exe = String::from_utf8_lossy(&bytes)
                    .trim_end_matches('\0')
                    .to_owned();
                ProcessInfo {
                    pid,
                    started: at,
                    exe,
                    ..Default::default()
                }
            });
        p.parent_pid = number(record, "ParentId").unwrap_or(0) as u32;
        return Ok(Some(Event::ProcessStart(p)));
    }
    Ok(None)
}

// Windows kernel TCP/UDP version 2 common prefix. Do not guess future layouts.
fn decode_network(
    b: &[u8],
    version: u8,
    ipv6: bool,
    protocol: Protocol,
    kind: NetKind,
    at: i64,
) -> Result<Event> {
    if version != 2 {
        bail!("Unsupported kernel network event version {version}");
    }
    let addr_len = if ipv6 { 16 } else { 4 };
    if b.len() < 8 + 2 * addr_len + 4 {
        bail!("Truncated network event");
    }
    let pid = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let bytes = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let addr = |s: &[u8]| -> IpAddr {
        if ipv6 {
            Ipv6Addr::from(<[u8; 16]>::try_from(s).unwrap()).into()
        } else {
            Ipv4Addr::new(s[0], s[1], s[2], s[3]).into()
        }
    };
    let dest = addr(&b[8..8 + addr_len]);
    let source = addr(&b[8 + addr_len..8 + 2 * addr_len]);
    let off = 8 + 2 * addr_len;
    let dport = u16::from_be_bytes([b[off], b[off + 1]]);
    let sport = u16::from_be_bytes([b[off + 2], b[off + 3]]);
    let source = SocketAddr::new(source, sport);
    let dest = SocketAddr::new(dest, dport);
    // TCP's saddr/sport are LOCAL even for receive and accept (kernel MOF contract).
    // UDP uses packet source/destination, so its receive direction must be reversed.
    let (local, remote) = if protocol == Protocol::Udp && kind == NetKind::Receive {
        (dest, source)
    } else {
        (source, dest)
    };
    Ok(Event::Network {
        at,
        pid,
        protocol,
        local,
        remote,
        bytes,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kernel_receive_uses_payload_pid_and_network_order_ports() {
        let mut b = Vec::new();
        b.extend(42u32.to_le_bytes());
        b.extend(123u32.to_le_bytes());
        b.extend([192, 0, 2, 1]);
        b.extend([127, 0, 0, 1]);
        b.extend(443u16.to_be_bytes());
        b.extend(50000u16.to_be_bytes());
        match decode_network(&b, 2, false, Protocol::Tcp, NetKind::Receive, 0).unwrap() {
            Event::Network {
                pid,
                local,
                remote,
                bytes,
                ..
            } => {
                assert_eq!(pid, 42);
                assert_eq!(local.to_string(), "127.0.0.1:50000");
                assert_eq!(remote.to_string(), "192.0.2.1:443");
                assert_eq!(bytes, 123);
            }
            _ => unreachable!(),
        }
        assert!(decode_network(&b[..10], 2, false, Protocol::Tcp, NetKind::Send, 0).is_err());
        assert!(decode_network(&b, 9, false, Protocol::Tcp, NetKind::Send, 0).is_err());
    }
}
