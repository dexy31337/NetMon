use super::{
    DECK, DECK_HI, INK, INK_DIM, INK_FAINT, NEON, NEON_SOFT, PULSE, RX, WELL, caption, display,
    glow, line, time,
};
use crate::model::{Outcome, Snapshot, now_ms};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Ui, pos2, vec2};

const BINS: usize = 48;
const BUCKETS_MS: [i64; 19] = [
    1, 10, 100, 250, 500, 1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000, 300_000,
    600_000, 900_000, 1_800_000, 3_600_000, 7_200_000,
];

#[derive(Default)]
pub(super) struct Overview {
    pub sent: u64,
    pub received: u64,
    pub failed: usize,
    pub dns_failed: usize,
    start: i64,
    end: i64,
    bucket_ms: i64,
    // First flow observations, DNS events, and observed error codes among both.
    counts: Vec<[u32; 3]>,
}

impl Overview {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        let Some(r) = &s.recording else {
            return Self::default();
        };
        let mut start = r.started;
        let mut end = r
            .ended
            .unwrap_or_else(|| if s.running { now_ms() } else { start });
        let mut totals = Self::default();
        for f in &s.flows {
            totals.sent += f.sent;
            totals.received += f.received;
            totals.failed += usize::from(f.outcome == Outcome::Failed);
            start = start.min(f.first);
            end = end.max(f.first).max(f.last);
        }
        for d in &s.dns {
            totals.dns_failed += usize::from(d.status.is_some_and(|status| status != 0));
            let at = d.query_time().unwrap_or(d.at);
            start = start.min(at);
            end = end.max(at);
        }
        totals.start = start;
        totals.end = end.max(start);
        let span = totals.end - start + 1;
        let wanted = (span + BINS as i64 - 1) / BINS as i64;
        totals.bucket_ms = BUCKETS_MS
            .into_iter()
            .find(|&duration| duration >= wanted)
            .unwrap_or_else(|| wanted.max(1));
        let bins = ((span + totals.bucket_ms - 1) / totals.bucket_ms).clamp(1, BINS as i64);
        totals.counts = vec![[0, 0, 0]; bins as usize];
        for f in &s.flows {
            let bucket = ((f.first - start) / totals.bucket_ms) as usize;
            totals.counts[bucket][0] += 1;
            totals.counts[bucket][2] += u32::from(f.outcome == Outcome::Failed);
        }
        for d in &s.dns {
            let bucket = ((d.query_time().unwrap_or(d.at) - start) / totals.bucket_ms) as usize;
            totals.counts[bucket][1] += 1;
            totals.counts[bucket][2] += u32::from(d.status.is_some_and(|status| status != 0));
        }
        totals
    }

    pub fn timeline(&self, ui: &mut Ui) {
        egui::Frame::new()
            .fill(DECK)
            .stroke(line(1.0, glow(NEON, 0.13)))
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                let narrow = ui.available_width() < 610.0;
                let stacked_header = ui.available_width() < 900.0;
                let heading = |ui: &mut Ui| {
                    ui.label(caption("АКТИВНОСТЬ ПО ВРЕМЕНИ").color(INK));
                    ui.add_space(7.0);
                    ui.label(
                        egui::RichText::new("↑ Первые наблюдения соединений")
                            .small()
                            .color(NEON_SOFT),
                    );
                    ui.label(egui::RichText::new("↓ События DNS").small().color(RX));
                    ui.label(egui::RichText::new("◆ Коды ошибок").small().color(PULSE));
                };
                if stacked_header {
                    ui.horizontal_wrapped(heading);
                } else {
                    ui.horizontal(|ui| {
                        heading(ui);
                        if !self.counts.is_empty() {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} — {} · интервал {}",
                                            time(self.start),
                                            time(self.end),
                                            duration_label(self.bucket_ms)
                                        ))
                                        .monospace()
                                        .small()
                                        .color(INK_DIM),
                                    );
                                },
                            );
                        }
                    });
                }
                if self.counts.is_empty() || self.counts.iter().all(|counts| *counts == [0, 0, 0]) {
                    let (rect, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), if narrow { 82.0 } else { 94.0 }),
                        Sense::hover(),
                    );
                    let p = ui.painter_at(rect);
                    p.rect_filled(rect, 0, WELL);
                    p.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Нет событий для временной шкалы\nОжидаются первые наблюдения соединений и события DNS",
                        FontId::proportional(13.0),
                        INK_DIM,
                    );
                    return;
                }
                if stacked_header {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} — {} · интервал {}",
                            time(self.start),
                            time(self.end),
                            duration_label(self.bucket_ms)
                        ))
                        .monospace()
                        .small()
                        .color(INK_DIM),
                    );
                }
                let peak = self
                    .counts
                    .iter()
                    .map(|counts| counts[0].max(counts[1]))
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let chart_height = if narrow { 96.0 } else { 124.0 };
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), chart_height), Sense::hover());
                let p = ui.painter_at(rect);
                let plot = Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() - 18.0));
                let data = Rect::from_min_max(
                    pos2(plot.left(), plot.top() + 17.0),
                    plot.right_bottom(),
                );
                let mid = data.center().y;
                let half_height = data.height() * 0.43;
                let step = plot.width() / self.counts.len() as f32;
                p.rect_filled(plot, 0, WELL);
                for i in 0..=4 {
                    let x = plot.left() + plot.width() * i as f32 / 4.0;
                    p.vline(x, plot.y_range(), line(1.0, glow(INK_DIM, 0.15)));
                }
                for fraction in [-0.5, 0.0, 0.5] {
                    let y = mid + fraction * half_height;
                    p.hline(data.x_range(), y, line(1.0, glow(INK_DIM, 0.25)));
                }
                p.text(
                    plot.left_top() + vec2(5.0, 4.0),
                    Align2::LEFT_TOP,
                    format!("макс. {peak} / интервал"),
                    FontId::monospace(10.5),
                    INK_FAINT,
                );
                for (i, pair) in self.counts.iter().enumerate() {
                    let x = plot.left() + i as f32 * step;
                    let inset = (step * 0.16).clamp(0.5, 2.0);
                    for (count, direction, color) in [(pair[0], -1.0, NEON), (pair[1], 1.0, RX)] {
                        if count == 0 {
                            continue;
                        }
                        let h = (half_height * count as f32 / peak as f32).max(2.0);
                        let bar = Rect::from_two_pos(
                            pos2(x + inset, mid),
                            pos2(x + step - inset, mid + direction * h),
                        );
                        p.rect_filled(bar, 0.0, glow(color, 0.78));
                    }
                    if pair[2] > 0 {
                        let flow_h = half_height * pair[0] as f32 / peak as f32;
                        p.circle_filled(
                            pos2(x + step * 0.5, mid - flow_h - 4.0),
                            2.5,
                            PULSE,
                        );
                    }
                }
                let label_y = rect.bottom() - 7.0;
                for (x, align, at) in [
                    (rect.left(), Align2::LEFT_CENTER, self.start),
                    (rect.center().x, Align2::CENTER_CENTER, self.start + (self.end - self.start) / 2),
                    (rect.right(), Align2::RIGHT_CENTER, self.end),
                ] {
                    p.text(pos2(x, label_y), align, time(at), FontId::monospace(10.5), INK_FAINT);
                }
                if let Some(pos) = response.hover_pos().filter(|pos| plot.contains(*pos)) {
                    let slot = (((pos.x - plot.left()) / step) as usize).min(self.counts.len() - 1);
                    let left = plot.left() + slot as f32 * step;
                    p.rect_filled(
                        Rect::from_min_size(pos2(left, plot.top()), vec2(step, plot.height())),
                        0,
                        glow(INK, 0.1),
                    );
                    let edge = |i: usize| self.start + i as i64 * self.bucket_ms;
                    let [flows, dns, errors] = self.counts[slot];
                    let stamp = |ms| {
                        chrono::DateTime::from_timestamp_millis(ms)
                            .map(|t| {
                                t.with_timezone(&chrono::Local)
                                    .format("%H:%M:%S%.3f")
                                    .to_string()
                            })
                            .unwrap_or_else(|| "—".into())
                    };
                    response.on_hover_text(format!(
                        "{} — {}\nПервые наблюдения соединений: {flows}\nСобытия DNS: {dns}\nНаблюдаемые коды ошибок: {errors}",
                        stamp(edge(slot)),
                        stamp((edge(slot + 1) - 1).min(self.end))
                    ));
                }
            });
    }
}

fn duration_label(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms} мс")
    } else if ms < 60_000 {
        format!("{} с", ms / 1_000)
    } else if ms < 3_600_000 {
        format!("{} мин", ms / 60_000)
    } else if ms < 3_600_000 * 24 {
        if ms % 3_600_000 == 0 {
            format!("{} ч", ms / 3_600_000)
        } else {
            format!("{} мин", ms / 60_000)
        }
    } else if ms % (3_600_000 * 24) == 0 {
        format!("{} сут", ms / (3_600_000 * 24))
    } else {
        format!("{} ч", ms / 3_600_000)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn summary_strip(
    ui: &mut Ui,
    connections: &str,
    failed: usize,
    sent: &str,
    sent_share: &str,
    received: &str,
    received_share: &str,
    dns: &str,
    dns_failed: usize,
) {
    egui::Frame::new()
        .fill(DECK)
        .stroke(line(1.0, glow(NEON, 0.13)))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.columns(4, |cols| {
                compact_metric(
                    &mut cols[0],
                    "СОЕДИНЕНИЯ",
                    connections,
                    INK,
                    &format!("Ошибок: {failed}"),
                    failed > 0,
                );
                compact_metric(&mut cols[1], "↑ TX", sent, NEON, sent_share, false);
                compact_metric(&mut cols[2], "↓ RX", received, RX, received_share, false);
                compact_metric(
                    &mut cols[3],
                    "DNS",
                    dns,
                    INK,
                    &format!("Ошибок: {dns_failed}"),
                    dns_failed > 0,
                );
            });
        });
}

fn compact_metric(
    ui: &mut Ui,
    title: &str,
    value: &str,
    color: Color32,
    detail: &str,
    alert: bool,
) {
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.label(caption(title));
    ui.horizontal(|ui| {
        ui.label(display(value, 20.0).color(color));
        ui.label(
            egui::RichText::new(detail)
                .monospace()
                .size(10.5)
                .color(if alert { PULSE } else { INK_FAINT }),
        );
    });
}

pub(super) fn share_meter(ui: &mut Ui, part: u64, total: u64, color: Color32) {
    let share = if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 15.0), Sense::hover());
    let track = Rect::from_min_size(
        pos2(rect.left(), rect.center().y - 2.0),
        vec2((rect.width() - 40.0).max(1.0), 4.0),
    );
    let p = ui.painter();
    p.rect_filled(track, 1, glow(color, 0.13));
    if part > 0 {
        p.rect_filled(
            Rect::from_min_size(
                track.min,
                vec2((track.width() * share as f32).max(1.0), track.height()),
            ),
            1,
            color,
        );
    }
    let label = if total == 0 {
        "—".into()
    } else {
        format!("{:.0}%", 100.0 * share)
    };
    p.text(
        rect.right_center(),
        Align2::RIGHT_CENTER,
        label,
        FontId::monospace(11.0),
        INK_DIM,
    );
}

pub(super) fn nav_icon(p: &egui::Painter, center: Pos2, index: usize, color: Color32) {
    let at = |x: f32, y: f32| center + vec2(x, y);
    let pen = line(1.45, color);
    let path = |points: &[(f32, f32)]| {
        p.add(Shape::line(
            points.iter().map(|&(x, y)| at(x, y)).collect(),
            pen,
        ));
    };
    match index {
        0 => {
            for y in [-4.0, 4.0] {
                p.rect_stroke(
                    Rect::from_center_size(at(0.0, y), vec2(16.0, 6.0)),
                    1,
                    pen,
                    egui::StrokeKind::Inside,
                );
                p.circle_filled(at(-4.5, y), 1.0, color);
            }
        }
        1 => {
            path(&[(-8.0, -4.0), (7.0, -4.0), (3.5, -7.5)]);
            path(&[(8.0, 4.0), (-7.0, 4.0), (-3.5, 7.5)]);
        }
        2 => {
            path(&[(-6.0, 0.0), (0.0, 0.0), (3.0, -5.0), (6.0, -5.0)]);
            path(&[(0.0, 0.0), (3.0, 5.0), (6.0, 5.0)]);
            for (x, y) in [(-7.0, 0.0), (7.0, -5.0), (7.0, 5.0)] {
                p.circle_filled(at(x, y), 2.0, color);
            }
        }
        _ => {
            p.circle_stroke(center, 7.5, pen);
            path(&[(0.0, -4.5), (0.0, 0.0), (3.5, 2.0)]);
        }
    }
}

/// Isometric processor and board traces for the idle view; purely schematic.
pub(super) fn idle_chip(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(330.0_f32.min(ui.available_width()), 126.0),
        Sense::hover(),
    );
    let p = ui.painter_at(rect);
    let center = rect.center();
    let at = |x: f32, y: f32| center + vec2(x, y);
    for i in -8i32..=8 {
        for j in -3i32..=3 {
            p.circle_filled(
                at(i as f32 * 19.0, j as f32 * 17.0),
                0.85,
                glow(INK_DIM, 0.23),
            );
        }
    }
    for sign in [-1.0, 1.0] {
        let color = if sign < 0.0 { NEON } else { RX };
        for n in 0..3 {
            let x = n as f32 * 12.0;
            let y = n as f32 * 13.0 - 10.0;
            let end = at(sign * (116.0 + x), y - 12.0);
            p.add(Shape::line(
                vec![
                    at(sign * (43.0 - x), y),
                    at(sign * (72.0 + x), y),
                    at(sign * (84.0 + x), y - 12.0),
                    end,
                ],
                line(1.2, glow(color, 0.56)),
            ));
            p.circle_stroke(end, 2.4, line(1.0, color));
        }
    }
    let diamond =
        |y: f32, w: f32, h: f32| vec![at(0.0, y - h), at(w, y), at(0.0, y + h), at(-w, y)];
    for (height, surface, stroke) in [
        (11.0, WELL, glow(RX, 0.6)),
        (5.0, DECK_HI, glow(NEON, 0.4)),
        (-3.0, DECK_HI, NEON_SOFT),
    ] {
        p.add(Shape::convex_polygon(
            diamond(height, 49.0, 26.0),
            surface,
            line(1.2, stroke),
        ));
    }
    p.add(Shape::convex_polygon(
        diamond(-3.0, 38.0, 19.0),
        DECK,
        line(1.0, glow(NEON, 0.42)),
    ));
    p.text(
        at(0.0, -4.0),
        Align2::CENTER_CENTER,
        "EXE",
        FontId::monospace(13.0),
        NEON_SOFT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CaptureConfig, DnsQuery, Recording};

    #[test]
    fn activity_counts_first_and_last_interval_and_handles_single_instant() {
        let mut s = Snapshot {
            recording: Some(Recording {
                id: "test".into(),
                started: 100,
                ended: Some(195),
                config: CaptureConfig {
                    exe_filters: vec![],
                    include_children: false,
                    udp_idle_seconds: 30,
                    label: String::new(),
                },
                health: Default::default(),
            }),
            ..Default::default()
        };
        for (at, status) in [(100, 0), (102, 0), (195, 9003)] {
            s.dns.push(DnsQuery {
                at,
                request_at: Some(at),
                status: Some(status),
                id: s.dns.len() as u64,
                process: Default::default(),
                name: "test.invalid".into(),
                query_type: 1,
                results: String::new(),
                duration_ms: None,
                source: String::new(),
            });
        }
        let overview = Overview::from_snapshot(&s);
        assert_eq!(overview.bucket_ms, 10);
        assert_eq!(overview.counts.len(), 10);
        assert_eq!(overview.dns_failed, 1);
        assert_eq!(overview.counts[0], [0, 2, 0]);
        assert_eq!(overview.counts[9], [0, 1, 1]);
        assert_eq!(
            overview.counts.iter().map(|counts| counts[1]).sum::<u32>(),
            3
        );
        s.dns.truncate(1);
        s.recording.as_mut().unwrap().ended = Some(100);
        assert_eq!(Overview::from_snapshot(&s).counts, [[0, 1, 0]]);
    }
}
