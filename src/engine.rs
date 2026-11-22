use crate::model::*;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FlowKey {
    pid: u32,
    started: i64,
    protocol: Protocol,
    local: SocketAddr,
    remote: SocketAddr,
}
#[derive(Default)]
struct Socket {
    pid: u32,
    protocol: u32,
    local: Option<SocketAddr>,
    flow: Option<usize>,
}

pub struct Engine {
    pub snapshot: Snapshot,
    pub dirty: HashSet<usize>,
    pub saved_dns: usize,
    processes: HashMap<u32, Vec<ProcessInfo>>,
    active: HashMap<FlowKey, usize>,
    sockets: HashMap<u64, Socket>,
    dns_pending: HashMap<(u32, String, u32, u128), Vec<i64>>,
    selected: HashSet<(u32, i64)>,
}

impl Engine {
    pub fn new(config: CaptureConfig, processes: Vec<ProcessInfo>) -> Self {
        let started = now_ms();
        let recording = Recording {
            id: format!(
                "{}-{}",
                chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f"),
                std::process::id()
            ),
            started,
            ended: None,
            config,
            health: Health::default(),
        };
        let mut this = Self {
            snapshot: Snapshot {
                recording: Some(recording),
                running: true,
                ..Default::default()
            },
            dirty: HashSet::new(),
            saved_dns: 0,
            processes: HashMap::new(),
            active: HashMap::new(),
            sockets: HashMap::new(),
            dns_pending: HashMap::new(),
            selected: HashSet::new(),
        };
        for p in processes {
            this.register(p);
        }
        this
    }

    pub fn register(&mut self, p: ProcessInfo) {
        let entries = self.processes.entry(p.pid).or_default();
        if let Some(old) = entries.iter_mut().find(|old| old.started == p.started) {
            if !p.path.is_empty() {
                *old = p;
            }
        } else {
            entries.push(p);
            entries.sort_by_key(|p| p.started);
        }
    }

    fn process(&mut self, pid: u32, at: i64) -> ProcessInfo {
        if let Some(p) = self
            .processes
            .get(&pid)
            .and_then(|v| v.iter().rev().find(|p| p.started <= at))
        {
            return p.clone();
        }
        if let Some(p) = crate::collector::process_info(pid) {
            // Never attribute an old event to a later reuse of the PID.
            if p.started <= at {
                self.register(p.clone());
                return p;
            }
        }
        ProcessInfo {
            pid,
            exe: format!("PID {pid} (не определён)"),
            ..Default::default()
        }
    }

    fn wanted(&mut self, p: &ProcessInfo) -> bool {
        let config = &self.snapshot.recording.as_ref().unwrap().config;
        if config.matches(p) || self.selected.contains(&(p.pid, p.started)) {
            self.selected.insert((p.pid, p.started));
            return true;
        }
        if !config.include_children {
            return false;
        }
        let mut child = p;
        let mut visited = HashSet::new();
        while child.parent_pid != 0 && visited.insert((child.pid, child.started)) {
            let Some(parent) = self
                .processes
                .get(&child.parent_pid)
                .and_then(|v| v.iter().rev().find(|a| a.started <= child.started))
            else {
                break;
            };
            if config.matches(parent) || self.selected.contains(&(parent.pid, parent.started)) {
                self.selected.insert((p.pid, p.started));
                return true;
            }
            child = parent;
        }
        false
    }

    #[allow(clippy::too_many_arguments)] // All fields form one new flow record.
    fn add_flow(
        &mut self,
        p: ProcessInfo,
        protocol: Protocol,
        local: Option<SocketAddr>,
        remote: SocketAddr,
        at: i64,
        outcome: Outcome,
        source: &str,
    ) -> usize {
        let index = self.snapshot.flows.len();
        self.snapshot.flows.push(Flow {
            id: index as u64 + 1,
            process: p,
            protocol,
            local,
            remote,
            first: at,
            last: at,
            ended: None,
            outcome,
            end_reason: String::new(),
            error: None,
            sent: 0,
            received: 0,
            sent_events: 0,
            received_events: 0,
            retransmit_bytes: 0,
            partial: outcome == Outcome::Unknown,
            source: source.into(),
        });
        self.dirty.insert(index);
        index
    }

    pub fn apply(&mut self, event: Event) {
        self.snapshot.recording.as_mut().unwrap().health.events += 1;
        match event {
            Event::ProcessStart(p) => self.register(p),
            Event::ProcessEnd { at, pid } => {
                for (i, flow) in self.snapshot.flows.iter_mut().enumerate() {
                    if flow.process.pid == pid && flow.first <= at && flow.ended.is_none() {
                        flow.ended = Some(at);
                        flow.end_reason = "Процесс завершён".into();
                        self.dirty.insert(i);
                    }
                }
            }
            Event::Network {
                at,
                pid,
                protocol,
                local,
                remote,
                bytes,
                kind,
            } => {
                let p = self.process(pid, at);
                if !self.wanted(&p) {
                    return;
                }
                let key = FlowKey {
                    pid,
                    started: p.started,
                    protocol,
                    local,
                    remote,
                };
                let existing = self.active.get(&key).copied().filter(|i| {
                    let f = &self.snapshot.flows[*i];
                    f.ended.is_none() || (kind == NetKind::Disconnect && at - f.last < 2000)
                });
                let establishing = matches!(kind, NetKind::Connect | NetKind::Accept);
                let mut index = existing;
                // A successful kernel connect can arrive before or after AFD completion.
                // Only merge a unique pending socket attempt; ambiguous attempts stay separate.
                if index.is_none() && protocol == Protocol::Tcp {
                    let candidates: Vec<_> = self
                        .sockets
                        .values()
                        .filter_map(|s| s.flow)
                        .filter(|i| {
                            let f = &self.snapshot.flows[*i];
                            f.process.pid == pid
                                && f.process.started == p.started
                                && f.remote == remote
                                && f.ended.is_none()
                                && f.local.is_none_or(|a| {
                                    a == local
                                        || a.port() == 0
                                        || (a.ip().is_unspecified() && a.port() == local.port())
                                })
                                && (at - f.first).abs() <= 120_000
                        })
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect();
                    if candidates.len() == 1 {
                        index = Some(candidates[0]);
                    }
                }
                if let Some(i) = index {
                    let f = &self.snapshot.flows[i];
                    // A second connect after observed traffic denotes a new generation.
                    if establishing
                        && f.outcome == Outcome::Established
                        && f.last + 1000 < at
                        && f.sent_events + f.received_events > 0
                    {
                        self.snapshot.flows[i].ended = Some(at);
                        self.snapshot.flows[i].end_reason =
                            "Повторное использование адресов; закрытие не наблюдалось".into();
                        self.snapshot.flows[i].partial = true;
                        self.dirty.insert(i);
                        index = None;
                    }
                }
                let i = index.unwrap_or_else(|| {
                    self.add_flow(
                        p,
                        protocol,
                        Some(local),
                        remote,
                        at,
                        if protocol == Protocol::Udp {
                            Outcome::Datagram
                        } else if establishing {
                            Outcome::Established
                        } else {
                            Outcome::Unknown
                        },
                        "Kernel ETW",
                    )
                });
                self.active.insert(key, i);
                let f = &mut self.snapshot.flows[i];
                f.local = Some(local);
                f.first = f.first.min(at);
                f.last = f.last.max(at);
                match kind {
                    NetKind::Send => {
                        f.sent += bytes as u64;
                        f.sent_events += 1;
                    }
                    NetKind::Receive => {
                        f.received += bytes as u64;
                        f.received_events += 1;
                    }
                    NetKind::Connect | NetKind::Accept => {
                        f.outcome = Outcome::Established;
                        f.partial = false;
                    }
                    NetKind::Disconnect => {
                        f.ended = Some(at);
                        f.end_reason = "Закрытие TCP наблюдалось".into();
                    }
                    NetKind::Retransmit => f.retransmit_bytes += bytes as u64,
                }
                self.dirty.insert(i);
            }
            Event::Dns {
                at,
                pid,
                name,
                query_type,
                status,
                results,
                activity,
                completed,
            } => {
                let p = self.process(pid, at);
                if !self.wanted(&p) {
                    return;
                }
                let key = (pid, name.to_lowercase(), query_type, activity);
                if !completed {
                    self.dns_pending.entry(key).or_default().push(at);
                    return;
                }
                let duration_ms = self
                    .dns_pending
                    .remove(&key)
                    .filter(|v| v.len() == 1)
                    .and_then(|v| (at >= v[0]).then_some(at - v[0]));
                self.snapshot.dns.push(DnsQuery {
                    id: self.snapshot.dns.len() as u64 + 1,
                    at,
                    process: p,
                    name,
                    query_type,
                    status,
                    results,
                    duration_ms,
                    source: "Microsoft-Windows-DNS-Client / 3008".into(),
                });
            }
            Event::SocketCreate {
                at: _,
                endpoint,
                pid,
                protocol,
            } => {
                self.sockets.insert(
                    endpoint,
                    Socket {
                        pid,
                        protocol,
                        ..Default::default()
                    },
                );
            }
            Event::SocketBind {
                endpoint, local, ..
            } => {
                if let Some(s) = self.sockets.get_mut(&endpoint) {
                    s.local = Some(local);
                }
            }
            Event::SocketConnect {
                at,
                endpoint,
                pid,
                remote,
                status,
            } => {
                let (owner, protocol, local) = self
                    .sockets
                    .get(&endpoint)
                    .map(|s| (s.pid, s.protocol, s.local))
                    .unwrap_or((pid, 0, None));
                // AFD also reports UDP connect(): it is not evidence of a TCP handshake.
                if protocol != 6 {
                    return;
                }
                let p = self.process(owner, at);
                if !self.wanted(&p) {
                    return;
                }
                let existing = self
                    .sockets
                    .get(&endpoint)
                    .and_then(|s| s.flow)
                    .filter(|i| {
                        let f = &self.snapshot.flows[*i];
                        f.remote == remote
                            && (f.ended.is_none()
                                || (status & 0x8000_0000 != 0 && at - f.last < 1000))
                    });
                let i = existing.unwrap_or_else(|| {
                    self.add_flow(
                        p,
                        Protocol::Tcp,
                        local,
                        remote,
                        at,
                        Outcome::Connecting,
                        "Winsock-AFD",
                    )
                });
                if let Some(s) = self.sockets.get_mut(&endpoint) {
                    s.flow = Some(i);
                }
                if status & 0x8000_0000 != 0 {
                    self.apply(Event::SocketResult {
                        at,
                        endpoint,
                        status,
                    });
                }
            }
            Event::SocketResult {
                at,
                endpoint,
                status,
            } => {
                if let Some(i) = self.sockets.get(&endpoint).and_then(|s| s.flow) {
                    let f = &mut self.snapshot.flows[i];
                    f.last = f.last.max(at);
                    if status == 0 {
                        f.outcome = Outcome::Established;
                    } else if status & 0x8000_0000 != 0 {
                        if f.outcome != Outcome::Established {
                            f.outcome = Outcome::Failed;
                        }
                        f.error = Some(status);
                        f.ended = Some(at);
                        f.end_reason = format!("AFD NTSTATUS 0x{status:08X}");
                    }
                    self.dirty.insert(i);
                }
            }
            Event::SocketClose { at, endpoint } => {
                if let Some(i) = self.sockets.remove(&endpoint).and_then(|s| s.flow) {
                    let f = &mut self.snapshot.flows[i];
                    if f.ended.is_none() {
                        f.ended = Some(at);
                        f.end_reason = "Сокет закрыт".into();
                        self.dirty.insert(i);
                    }
                }
            }
        }
    }

    pub fn tick(&mut self, at: i64) {
        let idle = self
            .snapshot
            .recording
            .as_ref()
            .unwrap()
            .config
            .udp_idle_seconds as i64
            * 1000;
        for (i, f) in self.snapshot.flows.iter_mut().enumerate() {
            if f.protocol == Protocol::Udp && f.ended.is_none() && at - f.last >= idle {
                f.ended = Some(f.last + idle);
                f.end_reason = "Период бездействия UDP".into();
                self.dirty.insert(i);
            }
        }
        self.active.retain(|_, i| {
            self.snapshot.flows[*i]
                .ended
                .is_none_or(|end| at - end < 2000)
        });
        self.dns_pending
            .retain(|_, v| v.last().is_some_and(|t| at - t < 120_000));
    }

    pub fn finish(&mut self) {
        let at = now_ms();
        self.snapshot.running = false;
        self.snapshot.recording.as_mut().unwrap().ended = Some(at);
        for (i, f) in self.snapshot.flows.iter_mut().enumerate() {
            if f.ended.is_none() {
                f.partial = true;
                f.end_reason = "Запись остановлена; завершение не наблюдалось".into();
                self.dirty.insert(i);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn engine() -> Engine {
        Engine::new(
            CaptureConfig {
                exe_filters: vec!["game.exe".into()],
                include_children: false,
                udp_idle_seconds: 30,
                label: "test".into(),
            },
            vec![ProcessInfo {
                pid: 42,
                started: 10,
                exe: "game.exe".into(),
                ..Default::default()
            }],
        )
    }

    #[test]
    fn afd_failure_and_kernel_close_are_one_attempt() {
        let mut e = engine();
        e.apply(Event::SocketCreate {
            at: 100,
            endpoint: 1,
            pid: 42,
            protocol: 6,
        });
        e.apply(Event::SocketBind {
            at: 101,
            endpoint: 1,
            local: "0.0.0.0:1234".parse().unwrap(),
        });
        e.apply(Event::SocketConnect {
            at: 102,
            endpoint: 1,
            pid: 42,
            remote: "127.0.0.1:5678".parse().unwrap(),
            status: 0,
        });
        e.apply(net(103, Protocol::Tcp, NetKind::Disconnect, 0));
        e.apply(Event::SocketResult {
            at: 104,
            endpoint: 1,
            status: 0xC0000236,
        });
        assert_eq!(e.snapshot.flows.len(), 1);
        assert_eq!(e.snapshot.flows[0].outcome, Outcome::Failed);
        assert_eq!(e.snapshot.flows[0].error, Some(0xC0000236));
    }

    #[test]
    fn established_result_survives_later_error() {
        let mut e = engine();
        e.apply(Event::SocketCreate {
            at: 100,
            endpoint: 1,
            pid: 42,
            protocol: 6,
        });
        e.apply(Event::SocketConnect {
            at: 101,
            endpoint: 1,
            pid: 42,
            remote: "127.0.0.1:5678".parse().unwrap(),
            status: 0,
        });
        e.apply(net(102, Protocol::Tcp, NetKind::Connect, 0));
        e.apply(net(103, Protocol::Tcp, NetKind::Send, 64));
        e.apply(Event::SocketResult {
            at: 104,
            endpoint: 1,
            status: 0xC000020D,
        });
        assert_eq!(e.snapshot.flows.len(), 1);
        assert_eq!(e.snapshot.flows[0].outcome, Outcome::Established);
        assert!(e.snapshot.flows[0].ended.is_some());
    }

    #[test]
    fn udp_connect_is_not_a_tcp_handshake() {
        let mut e = engine();
        e.apply(Event::SocketCreate {
            at: 100,
            endpoint: 1,
            pid: 42,
            protocol: 17,
        });
        e.apply(Event::SocketConnect {
            at: 101,
            endpoint: 1,
            pid: 42,
            remote: "127.0.0.1:5678".parse().unwrap(),
            status: 0,
        });
        e.apply(Event::SocketResult {
            at: 102,
            endpoint: 1,
            status: 0,
        });
        assert!(e.snapshot.flows.is_empty());
    }
    fn net(at: i64, protocol: Protocol, kind: NetKind, bytes: u32) -> Event {
        Event::Network {
            at,
            pid: 42,
            protocol,
            local: "127.0.0.1:1234".parse().unwrap(),
            remote: "127.0.0.1:5678".parse().unwrap(),
            bytes,
            kind,
        }
    }
    #[test]
    fn tcp_generations_and_retransmits() {
        let mut e = engine();
        e.apply(net(100, Protocol::Tcp, NetKind::Connect, 0));
        e.apply(net(110, Protocol::Tcp, NetKind::Send, 100));
        e.apply(net(120, Protocol::Tcp, NetKind::Retransmit, 100));
        e.apply(net(130, Protocol::Tcp, NetKind::Disconnect, 0));
        e.apply(net(140, Protocol::Tcp, NetKind::Connect, 0));
        assert_eq!(e.snapshot.flows.len(), 2);
        assert_eq!(e.snapshot.flows[0].sent, 100);
        assert_eq!(e.snapshot.flows[0].retransmit_bytes, 100);
        assert_eq!(e.snapshot.flows[0].outcome, Outcome::Established);
    }
    #[test]
    fn udp_no_reply_is_not_failure_and_idle_splits_flow() {
        let mut e = engine();
        e.apply(net(100, Protocol::Udp, NetKind::Send, 128));
        e.tick(30_101);
        e.apply(net(30_102, Protocol::Udp, NetKind::Send, 64));
        assert_eq!(e.snapshot.flows.len(), 2);
        assert_eq!(e.snapshot.flows[0].outcome, Outcome::Datagram);
    }
    #[test]
    fn selected_name_captures_each_instance_and_skips_unrelated_exes() {
        let mut e = engine();
        let connect = |at, pid| Event::Network {
            at,
            pid,
            protocol: Protocol::Tcp,
            local: "127.0.0.1:1234".parse().unwrap(),
            remote: "127.0.0.1:5678".parse().unwrap(),
            bytes: 0,
            kind: NetKind::Connect,
        };
        e.apply(connect(100, 42));
        for (pid, exe) in [(43, "GAME.EXE"), (44, "helper.exe")] {
            e.apply(Event::ProcessStart(ProcessInfo {
                pid,
                parent_pid: 42,
                started: 110,
                exe: exe.into(),
                path: format!(r"D:\Games\Alternate\{exe}"),
            }));
            e.apply(connect(120, pid));
        }
        e.apply(Event::ProcessEnd { at: 130, pid: 42 });
        e.apply(Event::ProcessStart(ProcessInfo {
            pid: 42,
            started: 140,
            exe: "Game.exe".into(),
            ..Default::default()
        }));
        e.apply(connect(150, 42));
        let instances: Vec<_> = e
            .snapshot
            .flows
            .iter()
            .map(|f| (f.process.pid, f.process.started))
            .collect();
        assert_eq!(instances, [(42, 10), (43, 110), (42, 140)]);
    }

    #[test]
    fn descendants_follow_the_parent_instance_at_each_birth() {
        for (original, replacement, expected) in [
            ("game.exe", "other.exe", true),
            ("other.exe", "game.exe", false),
        ] {
            let mut e = engine();
            e.snapshot
                .recording
                .as_mut()
                .unwrap()
                .config
                .include_children = true;
            for (pid, parent_pid, started, exe) in [
                (50, 0, 10, original),
                (51, 50, 20, "helper.exe"),
                (50, 0, 30, replacement),
                (52, 51, 40, "worker.exe"),
            ] {
                e.apply(Event::ProcessStart(ProcessInfo {
                    pid,
                    parent_pid,
                    started,
                    exe: exe.into(),
                    ..Default::default()
                }));
            }
            // Only the grandchild makes a DNS query. Neither ancestor has emitted traffic.
            e.apply(Event::Dns {
                at: 55,
                pid: 52,
                name: "test.invalid".into(),
                query_type: 1,
                status: 0,
                results: String::new(),
                activity: 0,
                completed: true,
            });
            assert_eq!(!e.snapshot.dns.is_empty(), expected);
        }
    }

    #[test]
    fn reused_pid_does_not_inherit_filter() {
        let mut e = engine();
        e.register(ProcessInfo {
            pid: 42,
            started: 200,
            exe: "other.exe".into(),
            ..Default::default()
        });
        e.apply(net(150, Protocol::Tcp, NetKind::Connect, 0));
        e.apply(net(250, Protocol::Tcp, NetKind::Connect, 0));
        assert_eq!(e.snapshot.flows.len(), 1);
        assert_eq!(e.snapshot.flows[0].process.started, 10);
    }
    #[test]
    fn ambiguous_dns_does_not_invent_duration() {
        let mut e = engine();
        for (at, completed) in [(100, false), (101, false), (110, true)] {
            e.apply(Event::Dns {
                at,
                pid: 42,
                name: "example.test".into(),
                query_type: 1,
                status: 0,
                results: "127.0.0.1".into(),
                activity: 0,
                completed,
            });
        }
        assert_eq!(e.snapshot.dns[0].duration_ms, None);
    }
}
