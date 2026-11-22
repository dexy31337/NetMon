mod instruments;
mod servers;
use crate::{
    collector,
    model::*,
    runtime::CaptureTask,
    storage::{self, Store},
};
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use instruments::{Overview, idle_chip, nav_icon, share_meter};
use serde::{Deserialize, Serialize};
use servers::{DnsCandidates, FlowFilter, ServerTarget, Servers, flow_search_matches};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

// Neon night-city palette. One violet-black hue for every surface; light only
// signals meaning: cyan is the observed signal and outgoing traffic, violet is
// incoming traffic, magenta is a failure, amber is incomplete data.
const VOID: Color32 = Color32::from_rgb(8, 7, 17);
const DECK: Color32 = Color32::from_rgb(15, 14, 31);
const DECK_HI: Color32 = Color32::from_rgb(23, 21, 46);
const WELL: Color32 = Color32::from_rgb(4, 4, 11);
const INK: Color32 = Color32::from_rgb(232, 234, 255);
const INK_DIM: Color32 = Color32::from_rgb(158, 160, 202);
const INK_FAINT: Color32 = Color32::from_rgb(141, 143, 178);
const NEON: Color32 = Color32::from_rgb(0, 240, 255);
const NEON_SOFT: Color32 = Color32::from_rgb(140, 247, 255);
const RX: Color32 = Color32::from_rgb(176, 148, 255);
const PULSE: Color32 = Color32::from_rgb(255, 46, 151);
const AMBER: Color32 = Color32::from_rgb(255, 184, 77);

const DISPLAY: &str = "display";
const RADIUS: u8 = 2;

fn glow(c: Color32, alpha: f32) -> Color32 {
    c.gamma_multiply(alpha)
}
fn line(width: f32, color: Color32) -> egui::Stroke {
    egui::Stroke::new(width, color)
}
fn grid() -> Color32 {
    glow(NEON, 0.13)
}

/// Bahnschrift: the condensed DIN face Windows ships, used for titles, figures and navigation.
fn display(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text)
        .family(egui::FontFamily::Name(DISPLAY.into()))
        .size(size)
}
/// Tracked uppercase caption, the voice of every label in the HUD.
fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .monospace()
        .size(10.5)
        .extra_letter_spacing(1.2)
        .color(INK_FAINT)
}
fn mono(text: impl Into<String>) -> RichText {
    RichText::new(text).monospace()
}

fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::dark();
    let r = egui::CornerRadius::same(RADIUS);
    v.panel_fill = VOID;
    v.window_fill = DECK_HI;
    v.extreme_bg_color = WELL;
    v.code_bg_color = WELL;
    v.faint_bg_color = glow(NEON, 0.035);
    v.hyperlink_color = NEON;
    v.warn_fg_color = AMBER;
    v.error_fg_color = PULSE;
    v.selection.bg_fill = glow(NEON, 0.16);
    v.selection.stroke = line(1.0, NEON_SOFT);
    v.text_cursor.stroke = line(2.0, NEON);
    v.window_corner_radius = egui::CornerRadius::same(3);
    v.menu_corner_radius = egui::CornerRadius::same(3);
    v.window_stroke = line(1.0, glow(NEON, 0.45));
    v.window_shadow = egui::Shadow {
        offset: [0, 0],
        blur: 28,
        spread: 0,
        color: glow(NEON, 0.16),
    };
    v.popup_shadow = egui::Shadow {
        offset: [0, 0],
        blur: 16,
        spread: 0,
        color: glow(NEON, 0.12),
    };
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = DECK;
    w.noninteractive.weak_bg_fill = DECK;
    w.noninteractive.bg_stroke = line(1.0, grid());
    w.noninteractive.fg_stroke = line(1.0, INK);
    w.inactive.bg_fill = WELL;
    w.inactive.weak_bg_fill = DECK;
    w.inactive.bg_stroke = line(1.0, glow(NEON, 0.22));
    w.inactive.fg_stroke = line(1.0, INK);
    w.hovered.bg_fill = DECK_HI;
    w.hovered.weak_bg_fill = DECK_HI;
    w.hovered.bg_stroke = line(1.0, glow(NEON, 0.7));
    w.hovered.fg_stroke = line(1.5, NEON_SOFT);
    w.active.bg_fill = glow(NEON, 0.2);
    w.active.weak_bg_fill = glow(NEON, 0.2);
    w.active.bg_stroke = line(1.0, NEON);
    w.active.fg_stroke = line(2.0, NEON);
    w.open = w.hovered;
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        s.corner_radius = r;
        s.expansion = 0.0;
    }
    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size.y = 28.0;
    for (text_style, font) in [
        (egui::TextStyle::Small, egui::FontId::proportional(11.0)),
        (egui::TextStyle::Body, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Button, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Monospace, egui::FontId::monospace(13.0)),
        (
            egui::TextStyle::Heading,
            egui::FontId::new(18.0, egui::FontFamily::Name(DISPLAY.into())),
        ),
    ] {
        style.text_styles.insert(text_style, font);
    }
    ctx.set_style(style);

    // Windows fonts give full Cyrillic coverage. The display family always exists,
    // falling back to the body face when Bahnschrift is missing.
    let mut fonts = egui::FontDefinitions::default();
    let mut load = |key: &str, file: &str| {
        std::fs::read(format!("C:\\Windows\\Fonts\\{file}"))
            .ok()
            .map(|bytes| {
                fonts
                    .font_data
                    .insert(key.into(), egui::FontData::from_owned(bytes).into());
                key.to_owned()
            })
    };
    let segoe = load("segoe", "segoeui.ttf");
    let bahn = load("bahnschrift", "bahnschrift.ttf");
    let consolas = load("consolas", "consola.ttf");
    let fam = &mut fonts.families;
    let proportional = fam.get_mut(&egui::FontFamily::Proportional).unwrap();
    proportional.splice(0..0, segoe);
    let body = proportional.clone();
    fam.get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .splice(0..0, consolas);
    fam.insert(
        egui::FontFamily::Name(DISPLAY.into()),
        bahn.into_iter().chain(body).collect(),
    );
    ctx.set_fonts(fonts);
}

/// HUD corner brackets: the signature mark on every framed readout.
fn brackets(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let l = 9.0;
    let s = line(1.5, color);
    let r = rect.shrink(0.5);
    for (corner, dx, dy) in [
        (r.left_top(), 1.0, 1.0),
        (r.right_top(), -1.0, 1.0),
        (r.left_bottom(), 1.0, -1.0),
        (r.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + egui::vec2(l * dx, 0.0)], s);
        painter.line_segment([corner, corner + egui::vec2(0.0, l * dy)], s);
    }
}

/// A framed readout: deck surface, hairline grid border, neon brackets.
fn panel<R>(
    ui: &mut egui::Ui,
    accent: Color32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let response = egui::Frame::new()
        .fill(DECK)
        .stroke(line(1.0, grid()))
        .corner_radius(RADIUS)
        .inner_margin(14.0)
        .show(ui, add);
    brackets(ui.painter(), response.response.rect, accent);
    response
}

/// Radar scope: the brand mark and the empty-state illustration.
fn radar(painter: &egui::Painter, center: egui::Pos2, radius: f32) {
    for k in [1.0, 0.66, 0.33] {
        painter.circle_stroke(
            center,
            radius * k,
            line(1.0, glow(NEON, 0.18 + 0.2 * (1.0 - k))),
        );
    }
    let s = line(1.0, glow(NEON, 0.1));
    painter.line_segment(
        [
            center - egui::vec2(radius, 0.0),
            center + egui::vec2(radius, 0.0),
        ],
        s,
    );
    painter.line_segment(
        [
            center - egui::vec2(0.0, radius),
            center + egui::vec2(0.0, radius),
        ],
        s,
    );
    let sweep = center + egui::vec2(0.72, -0.69) * radius;
    painter.line_segment([center, sweep], line(1.5, glow(NEON, 0.8)));
    let blip = center + egui::vec2(0.38, -0.2) * radius;
    for (r, a) in [(radius * 0.16, 0.12), (radius * 0.1, 0.3)] {
        painter.circle_filled(blip, r, glow(NEON, a));
    }
    painter.circle_filled(blip, (radius * 0.05).max(2.0), NEON);
}

/// Wraps a control in a coloured blur so it reads as lit from within.
fn lit<R>(ui: &mut egui::Ui, color: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .corner_radius(RADIUS)
        .shadow(egui::Shadow {
            offset: [0, 0],
            blur: 18,
            spread: 0,
            color: glow(color, 0.38),
        })
        .show(ui, add)
        .inner
}

fn head(ui: &mut egui::Ui, name: &str) {
    ui.label(caption(name.to_uppercase()));
}

fn protocol_tag(ui: &mut egui::Ui, protocol: Protocol) {
    let color = match protocol {
        Protocol::Tcp => NEON_SOFT,
        Protocol::Udp => RX,
    };
    // Painted at a fixed size so the tag never stretches to the row height.
    let text = galley(ui, mono(protocol.to_string()).size(11.0));
    let size = text.size() + egui::vec2(10.0, 4.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, RADIUS, glow(color, 0.07));
    p.rect_stroke(
        rect,
        RADIUS,
        line(1.0, glow(color, 0.45)),
        egui::StrokeKind::Inside,
    );
    p.galley(rect.min + egui::vec2(5.0, 2.0), text, color);
}

fn galley(ui: &egui::Ui, text: RichText) -> Arc<egui::Galley> {
    egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    )
}

/// Recording state lamp. Painted by hand so the lamp sits left of the text in
/// any parent layout.
fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32, live: bool) {
    let text = galley(ui, display(text, 13.0).extra_letter_spacing(1.5));
    let size = egui::vec2(text.size().x + 38.0, 30.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    if live {
        let shadow = egui::Shadow {
            offset: [0, 0],
            blur: 14,
            spread: 0,
            color: glow(color, 0.3),
        };
        p.add(shadow.as_shape(rect, egui::CornerRadius::same(RADIUS)));
    }
    p.rect_filled(rect, RADIUS, VOID);
    p.rect_filled(rect, RADIUS, glow(color, 0.08));
    p.rect_stroke(
        rect,
        RADIUS,
        line(1.0, glow(color, 0.55)),
        egui::StrokeKind::Inside,
    );
    let dot = rect.left_center() + egui::vec2(14.0, 0.0);
    if live {
        p.circle_filled(dot, 6.0, glow(color, 0.25));
    }
    p.circle_filled(dot, 3.5, color);
    let pos = egui::pos2(rect.left() + 26.0, rect.center().y - text.size().y / 2.0);
    p.galley(pos, text, color);
}

/// A glyph painted left of a centred button caption. The caption carries
/// leading spaces that reserve its room, so glyph and text centre together.
fn button_glyph(ui: &egui::Ui, button: &egui::Response, caption: RichText) -> egui::Pos2 {
    let width = galley(ui, caption).size().x;
    egui::pos2(
        button.rect.center().x - width / 2.0 + 6.0,
        button.rect.center().y,
    )
}

fn nav_item(ui: &mut egui::Ui, index: usize, name: &str, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, name)
    });
    let hovered = response.hovered() || response.has_focus();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, RADIUS, glow(NEON, 0.09));
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        p.rect_filled(bar.expand2(egui::vec2(3.0, 0.0)), 0.0, glow(NEON, 0.15));
        p.rect_filled(bar, 0.0, NEON);
    } else if hovered {
        p.rect_filled(rect, RADIUS, glow(NEON, 0.04));
    }
    let (icon, label) = if selected {
        (NEON, NEON_SOFT)
    } else if hovered {
        (INK_DIM, INK)
    } else {
        (INK_FAINT, INK_DIM)
    };
    let y = rect.center().y;
    nav_icon(p, egui::pos2(rect.left() + 22.0, y), index, icon);
    p.text(
        egui::pos2(rect.left() + 44.0, y),
        egui::Align2::LEFT_CENTER,
        name.to_uppercase(),
        egui::FontId::new(15.0, egui::FontFamily::Name(DISPLAY.into())),
        label,
    );
    response
}

#[derive(Serialize, Deserialize)]
struct Settings {
    #[serde(deserialize_with = "read_filters")]
    filters: Vec<String>,
    label: String,
    children: bool,
    udp_idle: u64,
    presets: Vec<CaptureConfig>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            label: "Наблюдение".into(),
            children: true,
            udp_idle: 30,
            presets: vec![],
        }
    }
}

// Earlier builds saved a delimited text field. New selections store each exact name.
fn read_filters<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Saved {
        Text(String),
        Names(Vec<String>),
    }
    let names = match Saved::deserialize(d)? {
        Saved::Text(s) => s.split([';', ',', '\n']).map(str::to_owned).collect(),
        Saved::Names(v) => v,
    };
    let mut seen = BTreeSet::new();
    Ok(names
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
        .collect())
}

struct App {
    dir: PathBuf,
    settings: Settings,
    capture: Option<CaptureTask>,
    live: Arc<Snapshot>,
    viewing: Option<Arc<Snapshot>>,
    tab: usize,
    search: String,
    server_drilldown: Option<ServerTarget>,
    protocol: usize,
    only_errors: bool,
    hide_loopback: bool,
    message: String,
    history: Vec<Recording>,
    active_history: std::collections::HashSet<String>,
    pending_history_delete: Option<String>,
    process_picker: ProcessPicker,
    selected_flow: Option<u64>,
    selected_dns: Option<u64>,
    stopping: bool,
    servers: Servers,
    expanded_servers: BTreeSet<IpAddr>,
    overview: Overview,
    overview_snapshot: std::sync::Weak<Snapshot>,
}

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1420.0, 900.0])
            .with_min_inner_size([1080.0, 680.0])
            .with_title("NetMon · Network Observatory"),
        ..Default::default()
    };
    eframe::run_native(
        "NetMon",
        options,
        Box::new(|cc| {
            apply_theme(&cc.egui_ctx);
            let dir = storage::data_dir();
            let settings = std::fs::read(dir.join("settings.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let mut app = App::new(dir, settings);
            app.refresh_history();
            Ok(Box::new(app))
        }),
    )
}

impl App {
    fn new(dir: PathBuf, settings: Settings) -> Self {
        Self {
            dir,
            settings,
            capture: None,
            live: Arc::new(Snapshot::default()),
            viewing: None,
            tab: 0,
            search: String::new(),
            server_drilldown: None,
            protocol: 0,
            only_errors: false,
            hide_loopback: false,
            message: String::new(),
            history: vec![],
            active_history: std::collections::HashSet::new(),
            pending_history_delete: None,
            process_picker: ProcessPicker::default(),
            selected_flow: None,
            selected_dns: None,
            stopping: false,
            servers: Servers::default(),
            expanded_servers: BTreeSet::new(),
            overview: Overview::default(),
            overview_snapshot: std::sync::Weak::new(),
        }
    }

    fn busy(&self) -> bool {
        self.capture.as_ref().is_some_and(|c| !c.finished())
    }
    fn snapshot(&self) -> Arc<Snapshot> {
        self.viewing.as_ref().unwrap_or(&self.live).clone()
    }
    fn config(&self) -> CaptureConfig {
        CaptureConfig {
            exe_filters: self.settings.filters.clone(),
            include_children: self.settings.children,
            udp_idle_seconds: self.settings.udp_idle,
            label: self.settings.label.clone(),
        }
    }
    fn save_settings(&mut self) {
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(
                self.dir.join("settings.json"),
                serde_json::to_vec_pretty(&self.settings)?,
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            self.message = format!("Не удалось сохранить настройки: {e}");
        }
    }
    fn refresh_history(&mut self) {
        match Store::open(&self.dir).and_then(|s| s.list()) {
            Ok(h) => {
                let current_recording_id = self
                    .capture
                    .as_ref()
                    .and_then(|_| self.live.recording.as_ref())
                    .map(|r| r.id.as_str());
                self.active_history = storage::active_recording_ids(&h, current_recording_id);
                self.history = h;
            }
            Err(e) => self.message = format!("История: {e:#}"),
        }
    }

    fn finish_history_delete(&mut self, id: &str, confirmed: bool) {
        if !confirmed {
            self.pending_history_delete = None;
            return;
        }
        let current_id = self
            .capture
            .as_ref()
            .and_then(|_| self.live.recording.as_ref())
            .map(|r| r.id.as_str());
        match Store::open(&self.dir).and_then(|mut store| store.delete(id, current_id)) {
            Ok(true) => {
                let was_viewing = self.viewing.as_ref().is_some_and(|snapshot| {
                    snapshot.recording.as_ref().is_some_and(|r| r.id == id)
                });
                let was_live = self
                    .live
                    .recording
                    .as_ref()
                    .is_some_and(|recording| recording.id == id);
                let was_displayed = was_viewing || (self.viewing.is_none() && was_live);
                if was_viewing {
                    self.viewing = None;
                }
                if was_live {
                    self.live = Arc::new(Snapshot::default());
                }
                if was_displayed {
                    self.server_drilldown = None;
                    self.expanded_servers.clear();
                    self.selected_flow = None;
                    self.selected_dns = None;
                    self.search.clear();
                    self.overview_snapshot = std::sync::Weak::new();
                }
                self.pending_history_delete = None;
                self.refresh_history();
                self.message = "Сохранённая запись удалена.".into();
            }
            Ok(false) => {
                self.pending_history_delete = None;
                self.refresh_history();
                self.message = "Запись не удалена: она активна или уже отсутствует.".into();
            }
            Err(e) => self.message = format!("Не удалось удалить запись: {e:#}"),
        }
    }

    fn start(&mut self) {
        if !collector::is_elevated() {
            self.message = "Для записи запустите NetMon от имени администратора. История доступна без повышения прав.".into();
            return;
        }
        let config = self.config();
        if config.exe_filters.is_empty() {
            self.message = "Выберите хотя бы одно приложение для записи.".into();
            return;
        }
        self.save_settings();
        self.capture = Some(CaptureTask::start(config, self.dir.clone()));
        self.live = Arc::new(Snapshot::default());
        self.viewing = None;
        self.server_drilldown = None;
        self.expanded_servers.clear();
        self.stopping = false;
        self.selected_flow = None;
        self.selected_dns = None;
        self.message.clear();
        if self.tab == 3 {
            self.tab = 0;
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        let busy = self.busy();
        egui::SidePanel::left("navigation")
            .exact_width(244.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(VOID).inner_margin(20.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                        radar(ui.painter(), rect.center(), 14.0);
                        ui.add_space(2.0);
                        ui.label(display("NETMON", 26.0).extra_letter_spacing(3.0).color(INK));
                    });
                    ui.add_space(16.0);
                    for (i, name) in ["Серверы", "Соединения", "DNS", "История"]
                        .iter()
                        .enumerate()
                    {
                        if nav_item(ui, i, name, self.tab == i).clicked() {
                            self.tab = i;
                            if i == 3 {
                                self.refresh_history();
                            }
                        }
                    }
                    ui.add_space(24.0);
                    ui.separator();
                    ui.add_space(12.0);
                    ui.label(caption("НОВАЯ ЗАПИСЬ").color(NEON_SOFT));
                    ui.add_space(4.0);
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.label(RichText::new("Название / группа").small().color(INK_DIM));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.label)
                                .desired_width(204.0),
                        );
                        self.process_picker.show(ui, &mut self.settings.filters);
                        ui.checkbox(&mut self.settings.children, "Включать дочерние процессы")
                            .on_hover_text("Записывать также процессы, запущенные выбранными приложениями, и их потомков.");
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Пауза UDP").color(INK_DIM));
                            ui.add(
                                egui::DragValue::new(&mut self.settings.udp_idle)
                                    .range(5..=600)
                                    .suffix(" с"),
                            );
                        });
                        ui.horizontal(|ui| {
                            if ui.small_button("Сохранить группу").clicked() {
                                let config = self.config();
                                if !config.exe_filters.is_empty() {
                                    self.settings.presets.retain(|p| p.label != config.label);
                                    self.settings.presets.push(config);
                                    self.save_settings();
                                }
                            }
                            ui.menu_button("Группы", |ui| {
                                for p in self.settings.presets.clone() {
                                    if ui.button(&p.label).clicked() {
                                        self.settings.filters = p.exe_filters;
                                        self.settings.label = p.label;
                                        self.settings.children = p.include_children;
                                        self.settings.udp_idle = p.udp_idle_seconds;
                                        ui.close_menu();
                                    }
                                }
                                if self.settings.presets.is_empty() {
                                    ui.label("Нет сохранённых групп");
                                }
                            });
                        });
                    });
                    ui.add_space(16.0);
                    let size = egui::vec2(ui.available_width(), 44.0);
                    if busy && self.stopping {
                        ui.add_enabled(
                            false,
                            egui::Button::new(
                                display("СОХРАНЕНИЕ…", 16.0).extra_letter_spacing(2.0),
                            )
                            .min_size(size),
                        );
                    } else if busy {
                        let text = display("      ОСТАНОВИТЬ", 16.0).extra_letter_spacing(2.0);
                        let stop = lit(ui, PULSE, |ui| {
                            ui.add(
                                egui::Button::new(text.clone().color(PULSE))
                                    .fill(glow(PULSE, 0.14))
                                    .stroke(line(1.0, PULSE))
                                    .min_size(size),
                            )
                        });
                        ui.painter().rect_filled(
                            egui::Rect::from_center_size(
                                button_glyph(ui, &stop, text),
                                egui::vec2(9.0, 9.0),
                            ),
                            1.0,
                            PULSE,
                        );
                        if stop.clicked()
                            && let Some(c) = &self.capture
                        {
                            c.stop();
                            self.stopping = true;
                        }
                    } else {
                        let text = display("      НАЧАТЬ ЗАПИСЬ", 16.0).extra_letter_spacing(2.0);
                        let start = lit(ui, NEON, |ui| {
                            ui.add(
                                egui::Button::new(text.clone().color(VOID))
                                    .fill(NEON)
                                    .stroke(line(1.0, NEON_SOFT))
                                    .min_size(size),
                            )
                        });
                        ui.painter()
                            .circle_filled(button_glyph(ui, &start, text), 4.5, VOID);
                        if start.clicked() {
                            self.start();
                        }
                    }
                    ui.add_space(18.0);
                    let (dot, text) = if collector::is_elevated() {
                        (NEON, "ADMIN · ЗАПИСЬ ДОСТУПНА")
                    } else {
                        (AMBER, "ТОЛЬКО ПРОСМОТР ИСТОРИИ")
                    };
                    let label = ui.label(caption(format!("   {text}")).color(INK_DIM));
                    ui.painter().circle_filled(
                        label.rect.left_center() + egui::vec2(3.0, 0.0),
                        3.0,
                        dot,
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Данные сохраняются локально.\nСодержимое пакетов не записывается.",
                        )
                        .small()
                        .color(INK_FAINT),
                    );
                });
            });
    }

    fn top(&mut self, ui: &mut egui::Ui, s: &Arc<Snapshot>) {
        let titles = [
            (
                "Серверы",
                "Адреса, к которым обращаются выбранные приложения",
            ),
            (
                "Соединения",
                "TCP-соединения и UDP-сессии за время наблюдения",
            ),
            (
                "DNS",
                "Результаты DNS-клиента Windows и прямых запросов приложений",
            ),
            ("История", "Локальные записи наблюдений"),
        ];
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    display(titles[self.tab].0.to_uppercase(), 30.0)
                        .extra_letter_spacing(1.5)
                        .color(INK),
                );
                ui.label(RichText::new(titles[self.tab].1).color(INK_DIM));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color, live) = if self.viewing.is_some() {
                    ("АРХИВ", RX, false)
                } else if self.stopping && self.busy() {
                    ("СОХРАНЕНИЕ", AMBER, false)
                } else if self.busy() {
                    ("REC · ЗАПИСЬ", NEON, true)
                } else if s.recording.as_ref().is_some_and(|r| r.ended.is_some()) {
                    ("ЗАВЕРШЕНО", INK_DIM, false)
                } else {
                    ("ОЖИДАНИЕ", INK_FAINT, false)
                };
                status_pill(ui, text, color, live);
            });
        });
        ui.add_space(16.0);
        if self.tab != 3 {
            if !std::ptr::eq(self.overview_snapshot.as_ptr(), Arc::as_ptr(s)) {
                self.overview = Overview::from_snapshot(s);
                self.overview_snapshot = Arc::downgrade(s);
            }
            let totals = &self.overview;
            let volume = totals.sent.saturating_add(totals.received);
            ui.columns(4, |cols| {
                metric(
                    &mut cols[0],
                    "СОЕДИНЕНИЯ",
                    &s.flows.len().to_string(),
                    INK,
                    |ui| {
                        metric_errors(ui, totals.failed, s.flows.len());
                    },
                );
                metric(
                    &mut cols[1],
                    "↑ TX · ОТПРАВЛЕНО",
                    &bytes(totals.sent),
                    NEON,
                    |ui| share_meter(ui, totals.sent, volume, NEON),
                );
                metric(
                    &mut cols[2],
                    "↓ RX · ПОЛУЧЕНО",
                    &bytes(totals.received),
                    RX,
                    |ui| share_meter(ui, totals.received, volume, RX),
                );
                metric(
                    &mut cols[3],
                    "DNS-ЗАПРОСЫ",
                    &s.dns.len().to_string(),
                    INK,
                    |ui| {
                        metric_errors(ui, totals.dns_failed, s.dns.len());
                    },
                );
            });
            totals.timeline(ui);
            ui.add_space(6.0);
        }
        if let Some(r) = &s.recording {
            ui.horizontal_wrapped(|ui| {
                ui.label(display(&r.config.label, 16.0).color(INK));
                ui.label(
                    mono(format!(
                        "{}  ·  {}",
                        timestamp(r.started),
                        r.config.exe_filters.join("; ")
                    ))
                    .color(INK_DIM),
                );
                if self.viewing.is_some() && ui.small_button("К текущей записи").clicked()
                {
                    self.viewing = None;
                    self.server_drilldown = None;
                    self.expanded_servers.clear();
                }
            });
            if r.health.incomplete() {
                alert(
                    ui,
                    AMBER,
                    &format!(
                        "Неполные данные: ETW {}, очередь {}, декодирование {}",
                        r.health.etw_lost, r.health.queue_lost, r.health.decode_errors
                    ),
                );
            }
            for message in &r.health.messages {
                alert(ui, AMBER, message);
            }
        }
        if let Some(error) = &s.error {
            alert(ui, PULSE, error);
        }
        if !self.message.is_empty() {
            ui.horizontal_wrapped(|ui| {
                alert(ui, AMBER, &self.message);
                if ui.small_button("×").clicked() {
                    self.message.clear();
                }
            });
        }
        ui.add_space(10.0);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, s: &Snapshot) {
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Поиск: IP, домен, EXE…")
                    .desired_width(265.0),
            );
            if (!self.search.is_empty() || (self.tab == 1 && self.server_drilldown.is_some()))
                && ui.button("Очистить фильтр поиска").clicked()
            {
                self.search.clear();
                self.server_drilldown = None;
                self.selected_flow = None;
                self.selected_dns = None;
            }
            if self.tab != 2 {
                egui::ComboBox::from_id_salt("protocol")
                    .selected_text(["TCP + UDP", "TCP", "UDP"][self.protocol])
                    .width(95.0)
                    .show_ui(ui, |ui| {
                        for (i, name) in ["TCP + UDP", "TCP", "UDP"].iter().enumerate() {
                            ui.selectable_value(&mut self.protocol, i, *name);
                        }
                    });
                ui.checkbox(&mut self.hide_loopback, "Без loopback");
            }
            ui.checkbox(&mut self.only_errors, "Ошибки");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(s.recording.is_some(), egui::Button::new("Экспорт записи"))
                    .on_hover_text(
                        "Вся запись: JSON и CSV; текущий поисковый фильтр не применяется",
                    )
                    .clicked()
                {
                    self.message = match storage::export(s, &self.dir) {
                        Ok(p) => format!("Экспорт сохранён: {}", p.display()),
                        Err(e) => format!("Экспорт: {e:#}"),
                    };
                }
            });
        });
        if self.tab == 1
            && let Some(target) = self.server_drilldown
        {
            ui.label(caption(format!("Соединения с {target}")).color(NEON_SOFT));
        }
        ui.add_space(12.0);
    }

    fn flow_filter(&self) -> FlowFilter {
        FlowFilter {
            protocol: match self.protocol {
                1 => Some(Protocol::Tcp),
                2 => Some(Protocol::Udp),
                _ => None,
            },
            only_errors: self.only_errors,
            hide_loopback: self.hide_loopback,
        }
    }

    fn connections(&mut self, ui: &mut egui::Ui, s: &Snapshot) {
        let query = self.search.trim().to_lowercase();
        let dns = if query.is_empty() {
            DnsCandidates::default()
        } else {
            DnsCandidates::new(&s.dns)
        };
        let filter = self.flow_filter();
        let rows: Vec<_> = s
            .flows
            .iter()
            .rev()
            .filter(|f| {
                filter.matches(f)
                    && self.server_drilldown.is_none_or(|target| target.matches(f))
                    && (query.is_empty() || flow_search_matches(f, &query, dns.for_flow(f)))
            })
            .collect();
        if rows.is_empty() {
            empty(
                ui,
                s,
                "Соединений пока нет",
                "Запустите запись, затем откройте игру или лаунчер.",
            );
            return;
        }
        ui.label(caption(format!(
            "{} строк · выберите строку для подробностей",
            rows.len()
        )));
        egui::ScrollArea::horizontal().show(ui, |ui| {
            TableBuilder::new(ui)
                .id_salt("connections-table")
                .sense(egui::Sense::click())
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(82.0))
                .column(Column::initial(170.0))
                .column(Column::initial(48.0))
                .column(Column::initial(210.0))
                .column(Column::initial(180.0))
                .column(Column::initial(95.0))
                .column(Column::initial(95.0))
                .column(Column::initial(100.0))
                .header(28.0, |mut h| {
                    for name in [
                        "Время",
                        "Приложение",
                        "Тип",
                        "Удалённый адрес",
                        "Результат",
                        "Отправлено",
                        "Получено",
                        "Наблюдение",
                    ] {
                        h.col(|ui| head(ui, name));
                    }
                })
                .body(|body| {
                    body.rows(30.0, rows.len(), |mut row| {
                        let f = rows[row.index()];
                        row.set_selected(self.selected_flow == Some(f.id));
                        row.col(|ui| {
                            ui.label(mono(time(f.first)).color(INK_FAINT));
                        });
                        row.col(|ui| {
                            ui.label(&f.process.exe).on_hover_text(format!(
                                "PID {}\n{}",
                                f.process.pid, f.process.path
                            ));
                        });
                        row.col(|ui| protocol_tag(ui, f.protocol));
                        row.col(|ui| {
                            ui.label(mono(f.remote.to_string()).color(INK));
                        });
                        row.col(|ui| {
                            let (mark, color) = match f.outcome {
                                Outcome::Failed => ("×", PULSE),
                                Outcome::Established => ("●", NEON),
                                Outcome::Datagram => ("◆", RX),
                                _ => ("○", INK_DIM),
                            };
                            ui.label(mono(mark).color(color));
                            ui.label(RichText::new(f.status()).color(
                                if f.outcome == Outcome::Failed {
                                    PULSE
                                } else {
                                    INK
                                },
                            ));
                        });
                        row.col(|ui| {
                            ui.label(mono(bytes(f.sent)).color(NEON_SOFT));
                        });
                        row.col(|ui| {
                            ui.label(mono(bytes(f.received)).color(RX));
                        });
                        row.col(|ui| {
                            let (text, color) = if f.ended.is_some() {
                                ("Завершено", INK_FAINT)
                            } else if s.running {
                                ("Активно*", NEON)
                            } else {
                                ("Прервано", INK_DIM)
                            };
                            ui.label(RichText::new(text).color(color));
                        });
                        if row.response().clicked() {
                            self.selected_flow = Some(f.id);
                        }
                    })
                });
        });
    }

    fn dns(&mut self, ui: &mut egui::Ui, s: &Snapshot) {
        let query = self.search.to_lowercase();
        let rows: Vec<_> = s
            .dns
            .iter()
            .rev()
            .filter(|d| {
                (!self.only_errors || d.status != 0)
                    && (query.is_empty()
                        || format!("{} {} {}", d.name, d.process.exe, d.results)
                            .to_lowercase()
                            .contains(&query))
            })
            .collect();
        if rows.is_empty() {
            let (title, hint) = if s.dns.is_empty() {
                (
                    "Результатов DNS пока нет",
                    "Запишите запуск приложения. Здесь появятся результаты DNS-клиента Windows и подтверждённые пары запрос–ответ по UDP/53 через IPv4. При собственном DoH, DoT или обращении по готовому IP имён может не быть.",
                )
            } else {
                (
                    "Нет результатов по фильтру",
                    "Очистите поиск или снимите флажок «Ошибки», чтобы увидеть сохранённые результаты DNS.",
                )
            };
            empty(ui, s, title, hint);
            return;
        }
        ui.label(RichText::new("Результаты DNS-клиента Windows и завершённые обмены по UDP/53 через IPv4. Источник указан в карточке запроса.").small().color(INK_FAINT));
        egui::ScrollArea::horizontal().show(ui, |ui| {
            TableBuilder::new(ui)
                .id_salt("dns-table")
                .sense(egui::Sense::click())
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(82.0))
                .column(Column::initial(165.0))
                .column(Column::initial(235.0))
                .column(Column::initial(50.0))
                .column(Column::initial(110.0))
                .column(Column::initial(300.0))
                .column(Column::initial(70.0))
                .header(28.0, |mut h| {
                    for name in [
                        "Время",
                        "Приложение",
                        "Имя",
                        "Тип",
                        "Результат",
                        "Ответ",
                        "Время, мс",
                    ] {
                        h.col(|ui| head(ui, name));
                    }
                })
                .body(|body| {
                    body.rows(30.0, rows.len(), |mut row| {
                        let d = rows[row.index()];
                        row.set_selected(self.selected_dns == Some(d.id));
                        row.col(|ui| {
                            ui.label(mono(time(d.at)).color(INK_FAINT));
                        });
                        row.col(|ui| {
                            ui.label(&d.process.exe);
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(&d.name).color(INK));
                        });
                        row.col(|ui| {
                            ui.label(mono(dns_type(d.query_type)).color(INK_DIM));
                        });
                        row.col(|ui| {
                            let ok = d.status == 0;
                            ui.label(
                                mono(format!(
                                    "{} {}",
                                    if ok { "●" } else { "×" },
                                    d.status_label()
                                ))
                                .color(if ok {
                                    NEON
                                } else {
                                    PULSE
                                }),
                            );
                        });
                        row.col(|ui| {
                            ui.label(mono(&d.results).color(INK_DIM))
                                .on_hover_text(&d.results);
                        });
                        row.col(|ui| {
                            ui.label(
                                mono(
                                    d.duration_ms
                                        .map(|x| x.to_string())
                                        .unwrap_or_else(|| "—".into()),
                                )
                                .color(INK_FAINT),
                            );
                        });
                        if row.response().clicked() {
                            self.selected_dns = Some(d.id);
                        }
                    })
                });
        });
    }

    fn history(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Обновить").clicked() {
                self.refresh_history();
            }
            ui.label(caption(format!("Хранилище: {}", self.dir.display())));
        });
        ui.add_space(12.0);
        if self.history.is_empty() {
            ui.label(
                RichText::new("Сохранённые записи появятся после первого запуска мониторинга.")
                    .color(INK_DIM),
            );
            return;
        }
        let history = self.history.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for r in &history {
                let accent = if r.ended.is_some() {
                    glow(NEON, 0.45)
                } else {
                    AMBER
                };
                panel(ui, accent, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(display(&r.config.label, 19.0).color(INK));
                            ui.label(
                                mono(format!(
                                    "{}  ·  {}",
                                    timestamp(r.started),
                                    r.config.exe_filters.join("; ")
                                ))
                                .color(INK_DIM),
                            );
                            let (text, color) = if r.ended.is_some() {
                                ("● ЗАПИСЬ ЗАВЕРШЕНА", INK_FAINT)
                            } else {
                                ("▲ ЗАПИСЬ АКТИВНА ИЛИ БЫЛА ПРЕРВАНА", AMBER)
                            };
                            ui.label(caption(text).color(color));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let active = self.active_history.contains(&r.id);
                            if ui
                                .add_enabled(!active, egui::Button::new("Удалить…"))
                                .on_disabled_hover_text(
                                    "Эта запись сейчас ведётся одной из копий NetMon.",
                                )
                                .clicked()
                            {
                                self.pending_history_delete = Some(r.id.clone());
                            }
                            if ui.button("Открыть").clicked() {
                                match Store::open(&self.dir).and_then(|s| s.load(&r.id)) {
                                    Ok(s) => {
                                        self.viewing = Some(Arc::new(s));
                                        self.tab = 0;
                                        self.search.clear();
                                        self.server_drilldown = None;
                                        self.expanded_servers.clear();
                                        self.selected_flow = None;
                                        self.selected_dns = None;
                                    }
                                    Err(e) => self.message = format!("История: {e:#}"),
                                }
                            }
                        });
                    });
                });
                ui.add_space(8.0);
            }
        });

        if let Some(id) = self.pending_history_delete.clone() {
            let recording = self.history.iter().find(|r| r.id == id).cloned();
            let mut confirm = false;
            let mut cancel = recording.is_none();
            if let Some(r) = recording {
                let modal = egui::Modal::new(egui::Id::new("delete-history-confirmation")).show(
                    ui.ctx(),
                    |ui| {
                        ui.set_max_width(460.0);
                        ui.label(display("Удалить сохранённую запись?", 21.0).color(INK));
                        ui.add_space(6.0);
                        ui.label(display(&r.config.label, 19.0).color(INK));
                        ui.label(mono(timestamp(r.started)).color(INK_DIM));
                        ui.add_space(8.0);
                        ui.label(
                            "Запись, все её соединения и DNS-запросы будут удалены без возможности восстановления.",
                        );
                        if r.ended.is_none() {
                            alert(
                                ui,
                                AMBER,
                                "Эта запись не завершена штатно, но сейчас не активна.",
                            );
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.button("Отмена").clicked() {
                                cancel = true;
                            }
                            if ui
                                .add(egui::Button::new("Удалить запись").fill(glow(PULSE, 0.24)))
                                .clicked()
                            {
                                confirm = true;
                            }
                        });
                    },
                );
                cancel |= modal.should_close();
            }
            if cancel {
                self.finish_history_delete(&id, false);
            } else if confirm {
                self.finish_history_delete(&id, true);
            }
        }
    }

    fn details(&mut self, ctx: &egui::Context, s: &Snapshot) {
        if self.tab == 1
            && let Some(f) = self
                .selected_flow
                .and_then(|id| s.flows.iter().find(|f| f.id == id))
        {
            let mut open = true;
            egui::Window::new("Сведения о соединении")
                .open(&mut open)
                .default_width(620.0)
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(display(&f.process.exe, 20.0).color(INK));
                        ui.label(display("→", 20.0).color(NEON));
                        ui.label(mono(f.remote.to_string()).size(16.0).color(NEON_SOFT));
                    });
                    ui.label(mono(&f.process.path).color(INK_FAINT));
                    ui.add_space(4.0);
                    ui.separator();
                    let failed = f.outcome == Outcome::Failed;
                    let rows = [
                        (
                            "ПРОЦЕСС",
                            format!(
                                "PID {} · запущен {}",
                                f.process.pid,
                                timestamp(f.process.started)
                            ),
                            INK,
                        ),
                        (
                            "МАРШРУТ",
                            format!(
                                "{}  {} → {}",
                                f.protocol,
                                f.local
                                    .map(|a| a.to_string())
                                    .unwrap_or_else(|| "не наблюдался".into()),
                                f.remote
                            ),
                            INK,
                        ),
                        ("РЕЗУЛЬТАТ", f.status(), if failed { PULSE } else { NEON }),
                        (
                            "ЗАВЕРШЕНИЕ",
                            if f.end_reason.is_empty() {
                                "Не наблюдалось".into()
                            } else {
                                f.end_reason.clone()
                            },
                            INK,
                        ),
                        ("ПЕРВОЕ СОБЫТИЕ", timestamp(f.first), INK_DIM),
                        ("ПОСЛЕДНЕЕ", timestamp(f.last), INK_DIM),
                        ("↑ TX", format!("{} байт", f.sent), NEON_SOFT),
                        ("↓ RX", format!("{} байт", f.received), RX),
                        (
                            "СОБЫТИЯ TX / RX",
                            format!("{} / {}", f.sent_events, f.received_events),
                            INK,
                        ),
                        (
                            "TCP-ПОВТОРЫ",
                            format!("{} байт (отдельно)", f.retransmit_bytes),
                            INK,
                        ),
                        ("ИСТОЧНИК", f.source.clone(), INK_DIM),
                    ];
                    egui::Grid::new("flow-details")
                        .num_columns(2)
                        .spacing([18.0, 6.0])
                        .show(ui, |ui| {
                            for (key, value, color) in rows {
                                ui.label(caption(key));
                                ui.label(mono(value).color(color));
                                ui.end_row();
                            }
                        });
                    if f.partial {
                        alert(
                            ui,
                            AMBER,
                            "Начало или завершение сессии не наблюдалось полностью.",
                        );
                    }
                    if let Some(code) = f.error {
                        alert(ui, PULSE, &format!("Код NTSTATUS: 0x{code:08X}"));
                    }
                    if ui.button("Копировать сведения").clicked() {
                        ui.ctx()
                            .copy_text(serde_json::to_string_pretty(f).unwrap_or_default());
                    }
                });
            if !open {
                self.selected_flow = None;
            }
        }
        if self.tab == 2
            && let Some(d) = self
                .selected_dns
                .and_then(|id| s.dns.iter().find(|d| d.id == id))
        {
            let mut open = true;
            egui::Window::new("Сведения о DNS-запросе")
                .open(&mut open)
                .default_width(620.0)
                .show(ctx, |ui| {
                    ui.label(display(&d.name, 20.0).color(INK));
                    ui.label(
                        mono(format!(
                            "{} · PID {} · {}",
                            d.process.exe,
                            d.process.pid,
                            timestamp(d.at)
                        ))
                        .color(INK_DIM),
                    );
                    ui.add_space(4.0);
                    ui.separator();
                    egui::Grid::new("dns-details")
                        .num_columns(2)
                        .spacing([18.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(caption("ТИП"));
                            ui.label(mono(dns_type(d.query_type)).color(INK));
                            ui.end_row();
                            ui.label(caption("РЕЗУЛЬТАТ"));
                            ui.label(
                                mono(format!("{} (код {})", d.status_label(), d.status))
                                    .color(if d.status == 0 { NEON } else { PULSE }),
                            );
                            ui.end_row();
                            ui.label(caption("ОТВЕТ"));
                            ui.label(mono(&d.results).color(INK));
                            ui.end_row();
                            ui.label(caption("ИСТОЧНИК"));
                            ui.label(mono(&d.source).color(INK_DIM));
                            ui.end_row();
                        });
                    if ui.button("Копировать сведения").clicked() {
                        ui.ctx()
                            .copy_text(serde_json::to_string_pretty(d).unwrap_or_default());
                    }
                });
            if !open {
                self.selected_dns = None;
            }
        }
    }
}

#[derive(Default)]
struct ProcessPicker {
    search: String,
    running: Vec<String>,
    refreshed: Option<std::time::Instant>,
}

impl ProcessPicker {
    fn show(&mut self, ui: &mut egui::Ui, filters: &mut Vec<String>) {
        ui.label(RichText::new("Приложения").small().color(INK_DIM));
        let caption = match filters.as_slice() {
            [] => "Выбрать приложения…".into(),
            [name] => name.clone(),
            names => format!("Выбрано приложений: {}", names.len()),
        };
        let popup_id = ui.make_persistent_id("exe-picker-popup");
        let button = ui.add_sized(
            [204.0, ui.spacing().interact_size.y],
            egui::Button::new(caption).shortcut_text("   ").truncate(),
        );
        if button.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup_id));
        }
        let arrow =
            button.rect.right_center() - egui::vec2(ui.spacing().button_padding.x + 5.0, 0.0);
        let dy = if ui.memory(|m| m.is_popup_open(popup_id)) {
            -2.0
        } else {
            2.0
        };
        ui.painter().add(egui::Shape::line(
            vec![
                arrow + egui::vec2(-4.0, -dy),
                arrow + egui::vec2(0.0, dy),
                arrow + egui::vec2(4.0, -dy),
            ],
            ui.style().interact(&button).fg_stroke,
        ));
        let screen = ui.ctx().screen_rect();
        let below = screen.bottom() - button.rect.bottom();
        let above = button.rect.top() - screen.top();
        const POPUP_HEIGHT: f32 = 340.0;
        let (placement, room) = if below >= POPUP_HEIGHT || below >= above {
            (egui::AboveOrBelow::Below, below)
        } else {
            (egui::AboveOrBelow::Above, above)
        };
        egui::popup::popup_above_or_below_widget(
            ui,
            popup_id,
            &button,
            placement,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                // egui restores a popup's previous content size. Size the viewport
                // from the window, independently of the last search result count.
                let margins = egui::Frame::popup(ui.style()).total_margin().sum();
                ui.set_width(280.0_f32.min(screen.width() - margins.x));
                ui.set_height((room.min(POPUP_HEIGHT) - margins.y).max(1.0));
                let opening = self.refreshed.is_none();
                if self
                    .refreshed
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
                {
                    self.running = collector::executable_names();
                    self.refreshed = Some(std::time::Instant::now());
                }
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .desired_width(ui.available_width())
                        .hint_text("Поиск или имя EXE…"),
                );
                if opening {
                    search.request_focus();
                }
                let query = self.search.trim().to_lowercase();
                let choices = picker_choices(&self.running, filters, &query);
                if (query.ends_with(".exe") || query == "*")
                    && !choices.iter().any(|(name, _)| name.to_lowercase() == query)
                    && ui
                        .button(format!("Добавить {}", self.search.trim()))
                        .clicked()
                {
                    filters.push(self.search.trim().to_owned());
                }
                if choices.is_empty() && !query.ends_with(".exe") && query != "*" {
                    ui.label("Совпадений нет");
                }
                if choices.is_empty() {
                    return;
                }
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                let mut list = egui::ScrollArea::vertical()
                    .id_salt("exe-rows")
                    .min_scrolled_height(0.0)
                    .auto_shrink([false, false]);
                if opening || search.changed() {
                    list = list.vertical_scroll_offset(0.0);
                }
                let row_height = ui.spacing().interact_size.y;
                list.show_rows(ui, row_height, choices.len(), |ui, rows| {
                    for (name, checked) in &choices[rows] {
                        let mut selected = *checked;
                        let response = ui
                            .push_id(name, |ui| ui.checkbox(&mut selected, name.as_str()))
                            .inner;
                        if response.on_hover_text(name).changed() {
                            if selected {
                                filters.push(name.clone());
                            } else {
                                let key = name.to_lowercase();
                                filters.retain(|n| n.to_lowercase() != key);
                            }
                        }
                    }
                });
            },
        );
        if !ui.memory(|m| m.is_popup_open(popup_id)) {
            self.refreshed = None;
            self.search.clear();
        }
        if filters.len() > 1 {
            ui.label(mono(filters.join(", ")).size(11.0).color(NEON_SOFT));
        }
    }
}

fn picker_choices(running: &[String], selected: &[String], query: &str) -> Vec<(String, bool)> {
    let mut choices = BTreeMap::new();
    for (names, checked) in [(selected, true), (running, false)] {
        for name in names {
            let key = name.to_lowercase();
            if key.contains(query) {
                choices
                    .entry(key)
                    .or_insert_with(|| (name.clone(), checked));
            }
        }
    }
    let mut rows: Vec<_> = choices.into_values().collect();
    rows.sort_by_key(|(_, selected)| !selected);
    rows
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(c) = &self.capture {
            self.live = c.snapshot();
            if c.finished() {
                self.capture = None;
                self.stopping = false;
                self.refresh_history();
            }
        }
        ctx.request_repaint_after(Duration::from_millis(if self.busy() { 300 } else { 1500 }));
        self.sidebar(ctx);
        let snapshot = self.snapshot();
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(WELL)
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(caption("NETMON 0.1 // WINDOWS 11"));
                    ui.separator();
                    ui.label(caption(
                        "Счётчики — наблюдаемые байты транспорта · Обновление ~2 с",
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(r) = &snapshot.recording {
                            let color = if snapshot.running { NEON } else { INK_DIM };
                            ui.label(
                                caption(format!("{} СОБЫТИЙ ETW", r.health.events)).color(color),
                            );
                        }
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(VOID).inner_margin(24.0))
            .show(ctx, |ui| {
                self.top(ui, &snapshot);
                if self.tab != 3 {
                    self.toolbar(ui, &snapshot);
                }
                match self.tab {
                    0 => self.servers(ui, &snapshot),
                    1 => self.connections(ui, &snapshot),
                    2 => self.dns(ui, &snapshot),
                    _ => self.history(ui),
                }
            });
        self.details(ctx, &snapshot);
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_settings();
        self.capture.take();
    }
}

fn metric(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    color: Color32,
    footer: impl FnOnce(&mut egui::Ui),
) {
    let accent = if color == INK {
        glow(NEON, 0.45)
    } else {
        color
    };
    panel(ui, accent, |ui| {
        ui.set_min_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(caption(title));
        ui.label(display(value, 30.0).color(color));
        footer(ui);
    });
}
fn metric_errors(ui: &mut egui::Ui, errors: usize, observations: usize) {
    if observations == 0 {
        ui.add_space(15.0);
    } else {
        let color = if errors > 0 { PULSE } else { INK_DIM };
        ui.label(mono(format!("Ошибок: {errors}")).size(11.0).color(color));
    }
}
fn alert(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(glow(color, 0.07))
        .stroke(line(1.0, glow(color, 0.35)))
        .corner_radius(RADIUS)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("▲  {text}")).color(color));
        });
}
fn empty(ui: &mut egui::Ui, s: &Snapshot, title: &str, body: &str) {
    ui.add_space((ui.available_height() * 0.12).clamp(4.0, 50.0));
    ui.vertical_centered(|ui| {
        idle_chip(ui);
        ui.add_space(6.0);
        ui.label(display(title, 24.0).color(INK));
        ui.add_space(4.0);
        ui.label(RichText::new(body).color(INK_DIM));
        if s.recording.is_some() {
            ui.label(
                RichText::new(
                    "Если запись содержит данные, попробуйте очистить поисковые фильтры.",
                )
                .small()
                .color(INK_FAINT),
            );
        }
    });
}
fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} Б")
    } else if n < 1024 * 1024 {
        format!("{:.1} КиБ", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.2} МиБ", n as f64 / 1048576.0)
    } else {
        format!("{:.2} ГиБ", n as f64 / 1073741824.0)
    }
}
fn time(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}
fn timestamp(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%d.%m.%Y %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    fn saved_recording(store: &mut Store, id: &str, started: i64) {
        let mut engine = crate::engine::Engine::new(
            CaptureConfig {
                exe_filters: vec!["test.exe".into()],
                include_children: false,
                udp_idle_seconds: 30,
                label: id.into(),
            },
            vec![],
        );
        let recording = engine.snapshot.recording.as_mut().unwrap();
        recording.id = id.into();
        recording.started = started;
        engine.finish();
        store.save(&mut engine).unwrap();
    }

    #[test]
    fn history_delete_cancel_then_confirm_clears_displayed_recording() {
        let dir = std::env::temp_dir().join(format!(
            "netmon-history-ui-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut store = Store::open(&dir).unwrap();
        saved_recording(&mut store, "older", 10);
        saved_recording(&mut store, "delete-me", 20);

        let mut app = App::new(dir.clone(), Settings::default());
        app.refresh_history();
        app.live = Arc::new(store.load("delete-me").unwrap());
        assert!(storage::export(&app.snapshot(), &dir).is_ok());

        app.pending_history_delete = Some("delete-me".into());
        app.finish_history_delete("delete-me", false);
        assert!(app.pending_history_delete.is_none());
        assert!(store.load("delete-me").is_ok());
        assert_eq!(app.snapshot().recording.as_ref().unwrap().id, "delete-me");

        app.pending_history_delete = Some("delete-me".into());
        app.finish_history_delete("delete-me", true);
        assert!(app.pending_history_delete.is_none());
        assert!(store.load("delete-me").is_err());
        assert!(app.snapshot().recording.is_none());
        assert!(storage::export(&app.snapshot(), &dir).is_err());
        assert_eq!(
            store
                .list()
                .unwrap()
                .into_iter()
                .map(|recording| recording.id)
                .collect::<Vec<_>>(),
            ["older"]
        );

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_choice_per_name_including_stopped_selections() {
        let running = ["GAME.exe", "game.exe", "Launcher.exe", "helper.exe"].map(String::from);
        let selected = ["Game.exe", "Game Editor.exe"].map(String::from);
        let choices = picker_choices(&running, &selected, "game");
        assert_eq!(
            choices,
            [
                (String::from("Game Editor.exe"), true),
                (String::from("Game.exe"), true)
            ]
        );
        assert_eq!(
            picker_choices(&running, &selected, ""),
            [
                ("Game Editor.exe".into(), true),
                ("Game.exe".into(), true),
                ("helper.exe".into(), false),
                ("Launcher.exe".into(), false),
            ]
        );
    }

    #[test]
    fn previous_settings_and_exact_exe_names_round_trip() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json["filters"] = " game.exe ;GAME.exe, Launcher.exe".into();
        json["children"] = false.into();
        let mut settings: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(settings.filters, ["game.exe", "Launcher.exe"]);
        assert!(!settings.children);
        settings.filters.push("Game, Editor; Preview.exe".into());
        let saved = serde_json::to_vec(&settings).unwrap();
        assert_eq!(
            serde_json::from_slice::<Settings>(&saved).unwrap().filters,
            settings.filters
        );
    }

    #[test]
    fn dropdown_search_and_selection_use_a_live_windows_executable() {
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let mut picker = ProcessPicker::default();
        let saved = "zz-NetMon-stopped.exe";
        let mut filters = vec![saved.to_owned()];
        let mut frame = |events: Vec<egui::Event>| {
            let output = ctx.run(
                egui::RawInput {
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1080.0, 680.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::SidePanel::left("navigation")
                        .exact_width(244.0)
                        .show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                ui.add_space(380.0);
                                picker.show(ui, &mut filters);
                            });
                        });
                },
            );
            let mut labels = Vec::new();
            for clipped in output.shapes {
                if let egui::Shape::Text(t) = clipped.shape {
                    let center = t.pos + t.galley.size() * 0.5;
                    if clipped.clip_rect.contains(center) {
                        labels.push((t.galley.job.text.clone(), center));
                    }
                }
            }
            labels
        };
        let locate = |labels: &[(String, egui::Pos2)], label: &str| {
            labels
                .iter()
                .find(|(name, _)| name == label)
                .unwrap_or_else(|| panic!("Missing {label:?} in {labels:?}"))
                .1
        };
        let click = |pos| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_str().unwrap();
        let open = locate(&frame(vec![]), saved);
        frame(click(open));
        frame(vec![]);
        frame(vec![]);
        let search = locate(&frame(vec![]), "Поиск или имя EXE…");
        frame(click(search));
        frame(vec![egui::Event::Text(name.to_uppercase())]);
        let choice = locate(&frame(vec![]), name);
        frame(click(choice));
        // The popup stays open for multiselect. Close it to check the saved selection.
        let outside = egui::pos2(700.0, 500.0);
        frame(click(outside));
        let reopen = locate(&frame(vec![]), "Выбрано приложений: 2");
        frame(click(reopen));
        frame(vec![]);
        let reopened = frame(vec![]);
        let running = collector::executable_names();
        let mut visible: Vec<_> = reopened
            .iter()
            .filter(|(text, _)| text == saved || running.contains(text))
            .collect();
        visible.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
        assert!(
            visible.len() >= 6,
            "Visible EXEs after reopening: {visible:?}"
        );
        assert!(visible[..2].iter().any(|(text, _)| text == saved));
        assert!(visible[..2].iter().any(|(text, _)| text == name));
        frame(click(locate(&reopened, saved)));
        frame(click(outside));
        frame(vec![]);
        assert_eq!(filters, [name]);
        assert!(picker.search.is_empty());
    }
}
