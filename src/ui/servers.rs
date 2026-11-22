use super::{
    App, INK, INK_DIM, INK_FAINT, NEON_SOFT, PULSE, RX, bytes, caption, empty, head, mono,
    protocol_tag,
};
use crate::model::{DnsQuery, Flow, Outcome, Protocol, Snapshot};
use eframe::egui::{self, RichText};
use egui_extras::{Column, TableBuilder};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FlowFilter {
    pub(super) protocol: Option<Protocol>,
    pub(super) only_errors: bool,
    pub(super) hide_loopback: bool,
}

impl FlowFilter {
    pub(super) fn matches(self, flow: &Flow) -> bool {
        self.protocol
            .is_none_or(|protocol| flow.protocol == protocol)
            && (!self.only_errors || flow.outcome == Outcome::Failed)
            && (!self.hide_loopback || !flow.remote.ip().is_loopback())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ServerPort {
    protocol: Protocol,
    port: u16,
}

#[derive(Clone, Copy)]
pub(super) struct ServerTarget {
    ip: IpAddr,
    port: Option<ServerPort>,
}

impl ServerTarget {
    fn address(self) -> String {
        self.port.map_or_else(
            || self.ip.to_string(),
            |port| SocketAddr::new(self.ip, port.port).to_string(),
        )
    }

    pub(super) fn matches(self, flow: &Flow) -> bool {
        self.ip == flow.remote.ip()
            && self.port.is_none_or(|port| {
                port.protocol == flow.protocol && port.port == flow.remote.port()
            })
    }

    fn copy_button(self, ui: &mut egui::Ui) -> egui::Response {
        let caption = if self.port.is_some() {
            "Скопировать IP и порт"
        } else {
            "Скопировать адрес"
        };
        let button = ui.small_button(caption);
        if button.clicked() {
            ui.ctx().copy_text(self.address());
        }
        button
    }
}

impl fmt::Display for ServerTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.address())?;
        if let Some(port) = self.port {
            write!(f, " ({})", port.protocol)?;
        }
        Ok(())
    }
}

type DnsKey = (u32, i64, IpAddr);

#[derive(Default)]
pub(super) struct DnsCandidates<'a>(HashMap<DnsKey, Vec<(&'a str, i64)>>);

impl<'a> DnsCandidates<'a> {
    pub(super) fn new(queries: &'a [DnsQuery]) -> Self {
        // Only successful answers from the same process instance can suggest a name.
        let mut candidates = Self::default();
        for query in queries.iter().filter(|query| query.status == 0) {
            for token in query.results.split([';', ' ', '\t', '\r', '\n']) {
                if let Ok(ip) = token.parse::<IpAddr>() {
                    candidates
                        .0
                        .entry((query.process.pid, query.process.started, ip.to_canonical()))
                        .or_default()
                        .push((&query.name, query.at));
                }
            }
        }
        candidates
    }

    pub(super) fn for_flow(&self, flow: &Flow) -> impl Iterator<Item = &'a str> + '_ {
        let first = flow.first;
        self.0
            .get(&(
                flow.process.pid,
                flow.process.started,
                flow.remote.ip().to_canonical(),
            ))
            .into_iter()
            .flatten()
            .filter(move |(_, at)| {
                (first.saturating_sub(300_000)..=first.saturating_add(100)).contains(at)
            })
            .map(|(name, _)| *name)
    }
}

pub(super) fn flow_search_matches<'a>(
    flow: &Flow,
    query: &str,
    mut names: impl Iterator<Item = &'a str>,
) -> bool {
    query.is_empty()
        || flow.process.exe.to_lowercase().contains(query)
        || flow.process.path.to_lowercase().contains(query)
        || flow.remote.to_string().contains(query)
        || flow
            .local
            .is_some_and(|local| local.to_string().contains(query))
        || names.any(|name| name.to_lowercase().contains(query))
}

#[derive(Default)]
struct ServerStats {
    processes: BTreeSet<String>,
    names: BTreeSet<String>,
    connections: usize,
    failed: usize,
    sent: u64,
    received: u64,
}

impl ServerStats {
    fn add<'a>(&mut self, flow: &Flow, names: impl Iterator<Item = &'a str>) {
        if !self.processes.contains(&flow.process.exe) {
            self.processes.insert(flow.process.exe.clone());
        }
        for name in names {
            if !self.names.contains(name) {
                self.names.insert(name.to_owned());
            }
        }
        self.connections += 1;
        self.failed += usize::from(flow.outcome == Outcome::Failed);
        self.sent += flow.sent;
        self.received += flow.received;
    }
}

struct Server {
    ip: IpAddr,
    total: ServerStats,
    ports: Vec<(ServerPort, ServerStats)>,
}

fn summarize_servers(snapshot: &Snapshot, filter: FlowFilter, query: &str) -> Vec<Server> {
    let dns = DnsCandidates::new(&snapshot.dns);
    let mut servers = BTreeMap::<IpAddr, (ServerStats, HashMap<ServerPort, ServerStats>)>::new();
    for flow in snapshot.flows.iter().filter(|flow| filter.matches(flow)) {
        if !flow_search_matches(flow, query, dns.for_flow(flow)) {
            continue;
        }
        let (total, ports) = servers.entry(flow.remote.ip()).or_default();
        total.add(flow, dns.for_flow(flow));
        ports
            .entry(ServerPort {
                protocol: flow.protocol,
                port: flow.remote.port(),
            })
            .or_default()
            .add(flow, dns.for_flow(flow));
    }
    let mut servers: Vec<_> = servers
        .into_iter()
        .map(|(ip, (total, ports))| {
            let mut ports: Vec<_> = ports.into_iter().collect();
            ports.sort_unstable_by_key(|(port, _)| (port.protocol == Protocol::Udp, port.port));
            Server { ip, total, ports }
        })
        .collect();
    servers.sort_unstable_by_key(|server| {
        (
            Reverse(u128::from(server.total.sent) + u128::from(server.total.received)),
            server.ip,
        )
    });
    servers
}

#[derive(Default)]
pub(super) struct Servers {
    rows: Vec<Server>,
    snapshot: Option<Arc<Snapshot>>,
    filter: FlowFilter,
    query: String,
}

impl Servers {
    fn rows(&mut self, snapshot: &Arc<Snapshot>, filter: FlowFilter, query: String) -> &[Server] {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, snapshot))
            || filter != self.filter
            || query != self.query
        {
            self.rows = summarize_servers(snapshot, filter, &query);
            self.snapshot = Some(snapshot.clone());
            self.filter = filter;
            self.query = query;
        }
        &self.rows
    }
}

impl App {
    pub(super) fn servers(&mut self, ui: &mut egui::Ui, snapshot: &Arc<Snapshot>) {
        let filter = self.flow_filter();
        let servers = self
            .servers
            .rows(snapshot, filter, self.search.trim().to_lowercase());
        if servers.is_empty() {
            empty(
                ui,
                snapshot,
                "Куда обращается приложение?",
                "Выберите EXE слева и начните запись. Здесь появятся серверы, доменные имена и объём трафика.",
            );
            return;
        }
        ui.label(caption(format!(
            "Адресов: {} · По объёму трафика · Домены — предположение по DNS за предыдущие 5 минут",
            servers.len()
        )));
        let mut rows = Vec::new();
        for server in servers {
            rows.push((server, None));
            if self.expanded_servers.contains(&server.ip) {
                rows.extend(server.ports.iter().map(|port| (server, Some(port))));
            }
        }
        let mut drilldown = None;
        egui::ScrollArea::horizontal().show(ui, |ui| {
            TableBuilder::new(ui)
                .id_salt("servers-table")
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(330.0))
                .column(Column::initial(160.0))
                .column(Column::initial(140.0))
                .column(Column::initial(60.0))
                .column(Column::initial(60.0))
                .column(Column::initial(85.0))
                .column(Column::initial(85.0))
                .header(28.0, |mut h| {
                    for name in [
                        "IP / протокол и порт",
                        "Возможные DNS-имена",
                        "Приложения",
                        "Сессии",
                        "Ошибки",
                        "Отправлено",
                        "Получено",
                    ] {
                        h.col(|ui| head(ui, name));
                    }
                })
                .body(|body| {
                    body.heterogeneous_rows(
                        rows.iter()
                            .map(|(_, port)| if port.is_some() { 32.0 } else { 52.0 }),
                        |mut row| {
                            let (server, port) = rows[row.index()];
                            let ip = server.ip;
                            row.col(|ui| {
                                if let Some((port, _)) = port {
                                    let target = ServerTarget {
                                        ip,
                                        port: Some(*port),
                                    };
                                    ui.add_space(20.0);
                                    protocol_tag(ui, port.protocol);
                                    if ui
                                        .link(mono(port.port.to_string()))
                                        .on_hover_text(
                                            "Показать соединения этого протокола и порта",
                                        )
                                        .clicked()
                                    {
                                        drilldown = Some(target);
                                    }
                                    target.copy_button(ui);
                                } else {
                                    let target = ServerTarget { ip, port: None };
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 3.0;
                                        ui.horizontal(|ui| {
                                            let expanded = self.expanded_servers.contains(&ip);
                                            if ports_toggle(ui, expanded).clicked() {
                                                if expanded {
                                                    self.expanded_servers.remove(&ip);
                                                } else {
                                                    self.expanded_servers.insert(ip);
                                                }
                                            }
                                            if ui
                                                .link(mono(ip.to_string()))
                                                .on_hover_text("Показать все соединения с этим IP")
                                                .clicked()
                                            {
                                                drilldown = Some(target);
                                            }
                                        });
                                        ui.horizontal(|ui| {
                                            ui.add_space(14.0);
                                            target.copy_button(ui);
                                            ui.label(caption(format!(
                                                "Пар протокол/порт: {}",
                                                server.ports.len()
                                            )));
                                        });
                                    });
                                }
                            });
                            let stats = port.map_or(&server.total, |(_, stats)| stats);
                            row.col(|ui| summary_list(ui, &stats.names, INK));
                            row.col(|ui| summary_list(ui, &stats.processes, INK_DIM));
                            row.col(|ui| {
                                ui.label(mono(stats.connections.to_string()).color(INK));
                            });
                            row.col(|ui| {
                                let color = if stats.failed > 0 { PULSE } else { INK_FAINT };
                                ui.label(mono(stats.failed.to_string()).color(color));
                            });
                            row.col(|ui| {
                                ui.label(mono(bytes(stats.sent)).color(NEON_SOFT))
                                    .on_hover_text(format!("{} Б", stats.sent));
                            });
                            row.col(|ui| {
                                ui.label(mono(bytes(stats.received)).color(RX))
                                    .on_hover_text(format!("{} Б", stats.received));
                            });
                        },
                    )
                });
        });
        if let Some(target) = drilldown {
            self.server_drilldown = Some(target);
            self.selected_flow = None;
            self.tab = 1;
        }
    }
}

fn ports_toggle(ui: &mut egui::Ui, expanded: bool) -> egui::Response {
    let (_, button) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
    let hint = if expanded {
        "Свернуть порты"
    } else {
        "Раскрыть порты"
    };
    button
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, expanded, hint));
    egui::collapsing_header::paint_default_icon(ui, if expanded { 1.0 } else { 0.0 }, &button);
    button.on_hover_text(hint)
}

fn summary_list(ui: &mut egui::Ui, values: &BTreeSet<String>, color: egui::Color32) {
    if values.is_empty() {
        ui.label(RichText::new("—").color(INK_FAINT));
    } else {
        let text = values
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        ui.add(egui::Label::new(RichText::new(&text).color(color)).truncate())
            .on_hover_text(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcessInfo;

    fn pointer_click(pos: egui::Pos2) -> Vec<egui::Event> {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![egui::Event::PointerMoved(pos), button(true), button(false)]
    }

    fn flow(remote: &str, protocol: Protocol, outcome: Outcome, sent: u64, received: u64) -> Flow {
        Flow {
            id: 0,
            process: ProcessInfo {
                pid: 1,
                started: 2,
                exe: "game.exe".into(),
                ..Default::default()
            },
            protocol,
            local: None,
            remote: remote.parse().unwrap(),
            first: 900_000,
            last: 900_010,
            ended: None,
            outcome,
            end_reason: String::new(),
            error: None,
            sent,
            received,
            sent_events: 0,
            received_events: 0,
            retransmit_bytes: 0,
            partial: false,
            source: String::new(),
        }
    }

    #[test]
    fn server_totals_cover_each_protocol_and_port_once() {
        let mut flows = vec![
            flow("192.0.2.1:443", Protocol::Tcp, Outcome::Established, 10, 30),
            flow("192.0.2.1:443", Protocol::Tcp, Outcome::Failed, 0, 0),
            flow("192.0.2.1:80", Protocol::Tcp, Outcome::Established, 5, 15),
            flow("192.0.2.1:443", Protocol::Udp, Outcome::Datagram, 20, 40),
            flow("192.0.2.10:443", Protocol::Tcp, Outcome::Established, 1, 2),
            flow("[::1]:443", Protocol::Udp, Outcome::Datagram, 3, 4),
        ];
        flows[3].process.exe = "voice.exe".into();
        let query = DnsQuery {
            id: 0,
            at: flows[0].first - 100,
            process: flows[0].process.clone(),
            name: "match.example".into(),
            query_type: 1,
            status: 0,
            results: "192.0.2.1".into(),
            duration_ms: None,
            source: String::new(),
        };
        let mut unrelated = query.clone();
        unrelated.process.started += 1;
        unrelated.name = "wrong-process.example".into();
        let mut too_old = query.clone();
        too_old.at = flows[0].first - 300_001;
        too_old.name = "old.example".into();
        let mut failed = query.clone();
        failed.status = 9003;
        failed.name = "failed.example".into();
        let snapshot = Snapshot {
            flows,
            dns: vec![query, unrelated, too_old, failed],
            ..Default::default()
        };

        let servers = summarize_servers(&snapshot, FlowFilter::default(), "");
        assert_eq!(servers.len(), 3);
        let server = &servers[0];
        assert_eq!(server.ip.to_string(), "192.0.2.1");
        assert_eq!(server.total.connections, 4);
        assert_eq!(server.total.failed, 1);
        assert_eq!(server.total.sent, 35);
        assert_eq!(server.total.received, 85);
        assert_eq!(
            server.total.processes,
            ["game.exe".into(), "voice.exe".into()].into()
        );
        assert_eq!(server.total.names, ["match.example".into()].into());
        let ports: Vec<_> = server
            .ports
            .iter()
            .map(|(port, stats)| {
                (
                    port.protocol,
                    port.port,
                    stats.connections,
                    stats.failed,
                    stats.sent,
                    stats.received,
                )
            })
            .collect();
        assert_eq!(
            ports,
            [
                (Protocol::Tcp, 80, 1, 0, 5, 15),
                (Protocol::Tcp, 443, 2, 1, 10, 30),
                (Protocol::Udp, 443, 1, 0, 20, 40),
            ]
        );

        let filtered = summarize_servers(
            &snapshot,
            FlowFilter {
                protocol: Some(Protocol::Tcp),
                only_errors: true,
                hide_loopback: true,
            },
            "match.example",
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].total.connections, 1);
        assert_eq!(filtered[0].ports.len(), 1);
        let without_loopback = summarize_servers(
            &snapshot,
            FlowFilter {
                hide_loopback: true,
                ..Default::default()
            },
            "",
        );
        assert_eq!(without_loopback.len(), 2);
    }

    #[test]
    fn server_targets_use_typed_addresses_and_copy_ipv6_with_port_syntax() {
        let tcp = flow(
            "[2001:db8::1]:443",
            Protocol::Tcp,
            Outcome::Established,
            10,
            30,
        );
        let udp = flow(
            "[2001:db8::1]:443",
            Protocol::Udp,
            Outcome::Datagram,
            10,
            30,
        );
        let other_port = flow(
            "[2001:db8::1]:4430",
            Protocol::Tcp,
            Outcome::Established,
            10,
            30,
        );
        let similar_ip = flow(
            "[2001:db8::10]:443",
            Protocol::Tcp,
            Outcome::Established,
            10,
            30,
        );
        let server = ServerTarget {
            ip: tcp.remote.ip(),
            port: None,
        };
        let port = ServerTarget {
            port: Some(ServerPort {
                protocol: Protocol::Tcp,
                port: 443,
            }),
            ..server
        };
        assert!(server.matches(&tcp));
        assert!(server.matches(&udp));
        assert!(server.matches(&other_port));
        assert!(!server.matches(&similar_ip));
        assert!(port.matches(&tcp));
        assert!(!port.matches(&udp));
        assert!(!port.matches(&other_port));
        assert!(!port.matches(&similar_ip));
        assert_eq!(server.address(), "2001:db8::1");
        assert_eq!(port.address(), "[2001:db8::1]:443");

        let v4_server = ServerTarget {
            ip: "192.0.2.1".parse().unwrap(),
            ..server
        };
        let similar_v4 = flow(
            "192.0.2.10:443",
            Protocol::Tcp,
            Outcome::Established,
            10,
            30,
        );
        assert!(!v4_server.matches(&similar_v4));
        assert_eq!(v4_server.address(), "192.0.2.1");
    }

    #[test]
    fn copy_buttons_send_the_address_to_the_clipboard() {
        let server = ServerTarget {
            ip: "2001:db8::1".parse().unwrap(),
            port: None,
        };
        let port = ServerTarget {
            port: Some(ServerPort {
                protocol: Protocol::Tcp,
                port: 443,
            }),
            ..server
        };
        for (target, expected) in [(server, "2001:db8::1"), (port, "[2001:db8::1]:443")] {
            let ctx = egui::Context::default();
            let frame = |events| {
                let mut center = egui::Pos2::ZERO;
                let output = ctx.run(
                    egui::RawInput {
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            center = target.copy_button(ui).rect.center();
                        });
                    },
                );
                (output, center)
            };
            let (_, pos) = frame(Vec::new());
            let (output, _) = frame(pointer_click(pos));
            assert!(
                output
                    .platform_output
                    .commands
                    .contains(&egui::OutputCommand::CopyText(expected.to_owned()))
            );
        }
    }

    #[test]
    fn port_link_and_clear_search_apply_to_the_visible_connections() {
        let snapshot = Arc::new(Snapshot {
            flows: [
                ("game-tcp.exe", "192.0.2.1:443", Protocol::Tcp),
                ("game-udp.exe", "192.0.2.1:443", Protocol::Udp),
                ("game-prefix.exe", "192.0.2.10:443", Protocol::Tcp),
                ("other.exe", "192.0.2.1:443", Protocol::Tcp),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (exe, address, protocol))| {
                let mut flow = flow(address, protocol, Outcome::Established, 1, 1);
                flow.id = (i + 1) as u64;
                flow.process.exe = exe.to_owned();
                flow
            })
            .collect(),
            ..Default::default()
        });
        for protocol in [0, 1] {
            let mut app = App::new(Default::default(), Default::default());
            app.search = "game-".into();
            app.protocol = protocol;
            app.expanded_servers.insert("192.0.2.1".parse().unwrap());
            let ctx = egui::Context::default();
            super::super::apply_theme(&ctx);
            let mut frame = |events| {
                let output = ctx.run(
                    egui::RawInput {
                        events,
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1350.0, 750.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            app.toolbar(ui, &snapshot);
                            if app.tab == 0 {
                                app.servers(ui, &snapshot);
                            } else {
                                assert_eq!(app.tab, 1);
                                app.connections(ui, &snapshot);
                            }
                        });
                    },
                );
                output
                    .shapes
                    .into_iter()
                    .filter_map(|clipped| {
                        if let egui::Shape::Text(text) = clipped.shape {
                            let center = text.pos + text.galley.size() * 0.5;
                            if clipped.clip_rect.contains(center) {
                                return Some((text.galley.job.text.clone(), center));
                            }
                        }
                        None
                    })
                    .collect::<Vec<_>>()
            };
            let locate = |labels: &[(String, egui::Pos2)], label: &str| {
                labels
                    .iter()
                    .find(|(text, _)| text == label)
                    .unwrap_or_else(|| panic!("Missing {label:?} in {labels:?}"))
                    .1
            };
            let connection_names = |labels: Vec<(String, egui::Pos2)>| {
                labels
                    .into_iter()
                    .map(|(name, _)| name)
                    .filter(|name| name.ends_with(".exe"))
                    .collect::<BTreeSet<_>>()
            };

            let server_labels = frame(Vec::new());
            // TCP appears before UDP for the expanded address, and other addresses are collapsed.
            frame(pointer_click(locate(&server_labels, "443")));
            let connection_labels = frame(Vec::new());
            let clear = locate(&connection_labels, "Очистить фильтр поиска");
            assert_eq!(
                connection_names(connection_labels),
                ["game-tcp.exe".into()].into()
            );
            frame(pointer_click(clear));
            let after_clear = connection_names(frame(Vec::new()));
            let mut expected = BTreeSet::from([
                "game-tcp.exe".into(),
                "game-prefix.exe".into(),
                "other.exe".into(),
            ]);
            if protocol == 0 {
                expected.insert("game-udp.exe".into());
            }
            assert_eq!(after_clear, expected);
            assert!(app.server_drilldown.is_none());
            assert!(app.search.is_empty());
            assert_eq!(app.protocol, protocol);
        }
    }
}
