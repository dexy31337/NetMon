use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub started: i64,
    pub exe: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    Tcp,
    Udp,
}
impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Unknown,
    Connecting,
    Established,
    Failed,
    Datagram,
}
impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Начало не наблюдалось",
            Self::Connecting => "Попытка подключения",
            Self::Established => "Установлено",
            Self::Failed => "Ошибка подключения",
            Self::Datagram => "Датаграммы",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Flow {
    pub id: u64,
    pub process: ProcessInfo,
    pub protocol: Protocol,
    pub local: Option<SocketAddr>,
    pub remote: SocketAddr,
    pub first: i64,
    pub last: i64,
    pub ended: Option<i64>,
    pub outcome: Outcome,
    pub end_reason: String,
    pub error: Option<u32>,
    pub sent: u64,
    pub received: u64,
    pub sent_events: u64,
    pub received_events: u64,
    pub retransmit_bytes: u64,
    pub partial: bool,
    pub source: String,
}
impl Flow {
    pub fn status(&self) -> String {
        if self.protocol == Protocol::Udp {
            match (self.sent_events > 0, self.received_events > 0) {
                (true, true) => "Трафик в обе стороны".into(),
                (true, false) => "Только отправка".into(),
                _ => "Только приём".into(),
            }
        } else {
            self.outcome.label().into()
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DnsQuery {
    pub id: u64,
    pub at: i64,
    pub process: ProcessInfo,
    pub name: String,
    pub query_type: u32,
    pub status: Option<u32>,
    pub results: String,
    pub duration_ms: Option<i64>,
    pub source: String,
    #[serde(default)]
    pub request_at: Option<i64>,
}
pub const UDP_DNS_SOURCE: &str = "UDP/53 · ";
impl DnsQuery {
    pub fn question(&self) -> String {
        format!("{} · {}", self.name, dns_type(self.query_type))
    }

    pub fn query_time(&self) -> Option<i64> {
        self.request_at
            .or_else(|| self.duration_ms.and_then(|ms| self.at.checked_sub(ms)))
    }

    pub fn status_label(&self) -> String {
        let Some(status) = self.status else {
            return "Ответ не наблюдался".into();
        };
        if self.source.starts_with(UDP_DNS_SOURCE) {
            return match status {
                0 => "OK".into(),
                1 => "FORMERR".into(),
                2 => "SERVFAIL".into(),
                3 => "NXDOMAIN".into(),
                4 => "NOTIMP".into(),
                5 => "REFUSED".into(),
                code => format!("RCODE {code}"),
            };
        }
        dns_status(status)
    }
}
pub fn dns_type(t: u32) -> String {
    match t {
        1 => "A".into(),
        28 => "AAAA".into(),
        5 => "CNAME".into(),
        12 => "PTR".into(),
        15 => "MX".into(),
        16 => "TXT".into(),
        33 => "SRV".into(),
        65 => "HTTPS".into(),
        _ => t.to_string(),
    }
}
pub fn dns_status(t: u32) -> String {
    match t {
        0 => "OK".into(),
        9003 => "NXDOMAIN".into(),
        9002 => "SERVFAIL".into(),
        9501 => "Нет записей".into(),
        1460 => "Тайм-аут".into(),
        _ => format!("Ошибка {t}"),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub exe_filters: Vec<String>,
    pub include_children: bool,
    pub udp_idle_seconds: u64,
    pub label: String,
}
impl CaptureConfig {
    pub fn matches(&self, process: &ProcessInfo) -> bool {
        self.exe_filters.iter().any(|f| {
            let f = f.trim().to_lowercase();
            f == "*"
                || (!f.is_empty()
                    && (f == process.exe.to_lowercase() || f == process.path.to_lowercase()))
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Health {
    pub events: u64,
    pub queue_lost: u64,
    pub etw_lost: u64,
    pub decode_errors: u64,
    #[serde(default)]
    pub dns_correlation_overflows: u64,
    pub messages: Vec<String>,
    #[serde(default)]
    pub source_events: std::collections::BTreeMap<String, u64>,
}
impl Health {
    pub fn incomplete(&self) -> bool {
        self.queue_lost + self.etw_lost + self.decode_errors + self.dns_correlation_overflows > 0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recording {
    pub id: String,
    pub started: i64,
    pub ended: Option<i64>,
    pub config: CaptureConfig,
    pub health: Health,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub recording: Option<Recording>,
    pub flows: Vec<Flow>,
    pub dns: Vec<DnsQuery>,
    pub running: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetKind {
    Send,
    Receive,
    Connect,
    Accept,
    Disconnect,
    Retransmit,
}

#[derive(Clone, Debug)]
pub enum Event {
    Network {
        at: i64,
        pid: u32,
        protocol: Protocol,
        local: SocketAddr,
        remote: SocketAddr,
        bytes: u32,
        kind: NetKind,
    },
    ProcessStart(ProcessInfo),
    ProcessEnd {
        at: i64,
        pid: u32,
    },
    WireDns {
        at: i64,
        packet: crate::dns_wire::Datagram,
    },
    Dns {
        at: i64,
        pid: u32,
        name: String,
        query_type: u32,
        status: u32,
        results: String,
        activity: u128,
        completed: bool,
    },
    SocketCreate {
        at: i64,
        endpoint: u64,
        pid: u32,
        protocol: u32,
    },
    SocketBind {
        at: i64,
        endpoint: u64,
        local: SocketAddr,
    },
    SocketConnect {
        at: i64,
        endpoint: u64,
        pid: u32,
        remote: SocketAddr,
        status: u32,
    },
    SocketResult {
        at: i64,
        endpoint: u64,
        status: u32,
    },
    SocketClose {
        at: i64,
        endpoint: u64,
    },
}

impl Event {
    pub fn at(&self) -> i64 {
        match self {
            Self::ProcessStart(p) => p.started,
            Self::Network { at, .. }
            | Self::ProcessEnd { at, .. }
            | Self::Dns { at, .. }
            | Self::WireDns { at, .. }
            | Self::SocketCreate { at, .. }
            | Self::SocketBind { at, .. }
            | Self::SocketConnect { at, .. }
            | Self::SocketResult { at, .. }
            | Self::SocketClose { at, .. } => *at,
        }
    }
}
