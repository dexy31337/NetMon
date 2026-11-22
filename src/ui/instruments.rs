use super::{DECK, DECK_HI, INK, INK_DIM, NEON, NEON_SOFT, RX, WELL, caption, glow, line, time};
use crate::model::{Outcome, Snapshot, now_ms};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Ui, pos2, vec2};

const BINS: usize = 48;

#[derive(Default)]
pub(super) struct Overview {
    pub sent: u64,
    pub received: u64,
    pub failed: usize,
    pub dns_failed: usize,
    start: i64,
    end: i64,
    // Each pair counts first observations of flows and completed DNS operations.
    counts: Vec<[u32; 2]>,
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
            totals.dns_failed += usize::from(d.status != 0);
            start = start.min(d.at);
            end = end.max(d.at);
        }
        totals.start = start;
        totals.end = end.max(start);
        let span = totals.end - start + 1;
        totals.counts = vec![[0, 0]; span.min(BINS as i64) as usize];
        for (at, series) in s
            .flows
            .iter()
            .map(|f| (f.first, 0))
            .chain(s.dns.iter().map(|d| (d.at, 1)))
        {
            let bucket = (at - start) * totals.counts.len() as i64 / span;
            totals.counts[bucket as usize][series] += 1;
        }
        totals
    }

    pub fn timeline(&self, ui: &mut Ui) {
        if self.counts.is_empty() {
            return;
        }
        let peak = self.counts.iter().flatten().copied().max().unwrap_or(0);
        if peak == 0 {
            return;
        }
        egui::Frame::new()
            .fill(DECK)
            .inner_margin(egui::Margin::symmetric(12, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(caption("АКТИВНОСТЬ ЗАПИСИ").color(INK));
                    ui.add_space(7.0);
                    ui.label(
                        egui::RichText::new("↑ Первые события потоков")
                            .small()
                            .color(NEON_SOFT),
                    );
                    ui.label(egui::RichText::new("↓ Результаты DNS").small().color(RX));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} — {}",
                                time(self.start),
                                time(self.end)
                            ))
                            .monospace()
                            .small()
                            .color(INK_DIM),
                        );
                    });
                });
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
                let p = ui.painter_at(rect);
                let mid = rect.center().y;
                let step = rect.width() / self.counts.len() as f32;
                p.rect_filled(rect, 0, WELL);
                for i in 0..=8 {
                    let x = rect.left() + rect.width() * i as f32 / 8.0;
                    p.vline(x, rect.y_range(), line(1.0, glow(INK_DIM, 0.15)));
                }
                p.hline(rect.x_range(), mid, line(1.0, glow(INK_DIM, 0.32)));
                for (i, pair) in self.counts.iter().enumerate() {
                    let x = rect.left() + i as f32 * step;
                    for (count, direction, color) in [(pair[0], -1.0, NEON), (pair[1], 1.0, RX)] {
                        if count == 0 {
                            continue;
                        }
                        let h = (11.0 * count as f32 / peak as f32).max(2.0);
                        let bar = Rect::from_two_pos(
                            pos2(x + 1.0, mid),
                            pos2(x + step - 1.0, mid + direction * h),
                        );
                        p.rect_filled(bar, 0.0, glow(color, 0.78));
                    }
                }
                if let Some(pos) = response.hover_pos() {
                    let slot = (((pos.x - rect.left()) / step) as usize).min(self.counts.len() - 1);
                    let left = rect.left() + slot as f32 * step;
                    p.rect_filled(
                        Rect::from_min_size(pos2(left, rect.top()), vec2(step, rect.height())),
                        0,
                        glow(INK, 0.1),
                    );
                    let span = self.end - self.start + 1;
                    let length = self.counts.len() as i64;
                    let edge = |i: usize| self.start + (i as i64 * span + length - 1) / length;
                    let [flows, dns] = self.counts[slot];
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
                        "{} — {}\nПервые события потоков: {flows}\nРезультаты DNS: {dns}",
                        stamp(edge(slot)),
                        stamp(edge(slot + 1) - 1)
                    ));
                }
            });
    }
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
                status,
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
        assert_eq!(overview.counts.len(), BINS);
        assert_eq!(overview.dns_failed, 1);
        for bucket in [0, 1, BINS - 1] {
            assert_eq!(overview.counts[bucket], [0, 1]);
        }
        assert_eq!(overview.counts.iter().flatten().sum::<u32>(), 3);
        s.dns.truncate(1);
        s.recording.as_mut().unwrap().ended = Some(100);
        assert_eq!(Overview::from_snapshot(&s).counts, [[0, 1]]);
    }
}
