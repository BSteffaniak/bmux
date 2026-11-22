//! Drawing the dashboard. Every view paints through [`Ui`] with the theme's colors, registers a
//! hit region for everything that responds to the pointer (see [`crate::app::Action::from_hit`]
//! for what each id does), and records the areas events need.

use std::time::Duration;

use bmux_tui::geometry::Rect;
use bmux_tui::hit::HitRole;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBarComponent, KeyHintBarStyles};
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarStyles, ProgressBarValue,
};
use bmux_tui_components::sparkline::{SparklineComponent, SparklineStyles};
use simvar_observer::{RunKind, SchedulerView};

use crate::app::{App, Page, Tab};
use crate::report::human;
use crate::store::RunRecord;
use crate::telemetry::{RunIndex, Tone};
use crate::theme::Theme;
use crate::ui::{Size, Ui, bar, columns, inset, row, rows, truncate};

/// A seed, short: its last eight hex digits.
pub fn short_seed(seed: u64) -> String {
    let hex = format!("{seed:x}");
    hex[hex.len().saturating_sub(8)..].to_owned()
}

/// `value` with thousands separators.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// How long ago, for people.
fn ago(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3600)
    }
}

fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[usize::try_from(tick % 8).unwrap_or(0)]
}

/// A line of one span.
fn one(span: Span) -> Line {
    Line::from_spans(vec![span])
}

fn span(text: impl Into<String>, style: Style) -> Span {
    Span::styled(text.into(), style)
}

/// A colored chip for a tag (`personality:windows` shows `windows` in its badge color).
fn chip(theme: &Theme, tag: &str) -> Span {
    let (key, value) = tag.split_once(':').unwrap_or(("", tag));
    let color = if key == "personality" {
        theme.personality(value)
    } else {
        theme.palette.info
    };
    span(
        format!(" {value} "),
        Style::new()
            .fg(Theme::color(theme.palette.background))
            .bg(Theme::color(color))
            .add_modifier(Modifier::BOLD),
    )
}

/// Draw the whole dashboard.
pub fn render(app: &App, cx: &mut PaintCx<'_, '_>) {
    let area = cx.area();
    let area = Rect::new(0, 0, area.width, area.height);
    let tick = u64::try_from(app.now.duration_since(app.started).as_millis() / 100).unwrap_or(0);
    let mut ui = Ui {
        cx,
        theme: &app.theme,
        hover: app.hover.as_deref(),
        areas: &app.areas,
        tick,
    };
    ui.fill(area, app.theme.canvas());
    let in_run = app.current_run().is_some();
    let parts = rows(
        area,
        &[
            Size::Fixed(1),
            Size::Fixed(1),
            Size::Fill(1),
            Size::Fixed(u16::from(in_run)),
            Size::Fixed(1),
        ],
    );
    header(app, &mut ui, parts[0]);
    tab_strip(app, &mut ui, parts[1]);
    match app.tabs.get(app.tab) {
        Some(Tab::Fleet) => fleet(app, &mut ui, parts[2]),
        Some(Tab::Failures) => failures(app, &mut ui, parts[2]),
        Some(Tab::Run(_)) => run(app, &mut ui, parts[2]),
        None => {}
    }
    if in_run {
        transport(app, &mut ui, parts[3]);
    }
    hints(app, &mut ui, parts[4]);
    toasts(app, &mut ui, area);
    if app.palette.is_some() {
        palette(app, &mut ui, area);
    }
    if app.help {
        help(app, &mut ui, area);
    }
}

fn header(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    ui.fill(area, theme.surface());
    let live = !app.store.finished;
    let dot = if live && ui.tick % 10 < 5 {
        "◆"
    } else {
        "◇"
    };
    ui.line(
        area,
        &Line::from_spans(vec![
            span(
                format!(" {dot} "),
                theme.accent().add_modifier(Modifier::BOLD),
            ),
            span("bmux sim", theme.strong()),
            span(" · ", theme.faint()),
            span(
                app.title.clone(),
                theme.accent().add_modifier(Modifier::BOLD),
            ),
        ]),
    );
    // Right side: fleet state, throughput, outcome counts, and controls.
    let status = app.controller.as_ref().map(|c| c.status());
    let mut right: Vec<Span> = Vec::new();
    match status {
        Some(state) if state.paused => right.push(span(
            "❚❚ paused ",
            theme.warn().add_modifier(Modifier::BOLD),
        )),
        Some(state) => {
            right.push(span("● ", theme.good()));
            right.push(span(format!("{} running ", state.active), theme.text()));
        }
        None if app.store.finished => right.push(span("■ done ", theme.muted())),
        None => right.push(span("● running ", theme.good())),
    }
    if let Some(rate) = app.store.runs_per_second.last() {
        right.push(span(format!("{rate} runs/s "), theme.info()));
    }
    right.push(span(
        format!("✔ {} ", grouped(app.store.totals.passed)),
        theme.good(),
    ));
    right.push(span(
        format!("✖ {} ", grouped(app.store.totals.failed)),
        if app.store.totals.failed > 0 {
            theme.bad().add_modifier(Modifier::BOLD)
        } else {
            theme.muted()
        },
    ));
    let text_width: u16 = right
        .iter()
        .map(|s| u16::try_from(s.width()).unwrap_or(0))
        .sum();
    let buttons = 22;
    let x = area.right().saturating_sub(text_width + buttons);
    ui.line(
        Rect::new(x, area.y, text_width, 1),
        &Line::from_spans(right),
    );
    let mut bx = x + text_width;
    if status.is_some() {
        let toggle = if status.is_some_and(|s| s.paused) {
            " ▶ "
        } else {
            " ❚❚ "
        };
        bx += ui.button("fleet.toggle", bx, area.y, toggle, theme.accent());
        bx += ui.button("fleet.fewer", bx, area.y, " − ", theme.text());
        bx += ui.button("fleet.more", bx, area.y, " + ", theme.text());
    }
    bx += ui.button("palette", bx, area.y, " ⌕ ", theme.text());
    bx += ui.button("theme", bx, area.y, " ◐ ", theme.text());
    ui.button("help", bx, area.y, " ? ", theme.text());
}

fn tab_strip(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    ui.fill(area, theme.canvas());
    let mut x = area.x + 1;
    for (index, tab) in app.tabs.iter().enumerate() {
        let (glyph, label, tone) = match tab {
            Tab::Fleet => ("▦", "Fleet".to_owned(), theme.info()),
            Tab::Failures => {
                let count = app.store.failures.len();
                (
                    if count > 0 { "✖" } else { "✔" },
                    if count > 0 {
                        format!("Failures {count}")
                    } else {
                        "Failures".into()
                    },
                    if count > 0 { theme.bad() } else { theme.good() },
                )
            }
            Tab::Run(run) => {
                let replay = run.travel.is_some();
                (
                    if replay { "◆" } else { "●" },
                    run.label.clone(),
                    if replay { theme.replay() } else { theme.good() },
                )
            }
        };
        let id = format!("tab.{index}");
        let selected = index == app.tab;
        let base = if selected {
            theme.raised().add_modifier(Modifier::BOLD)
        } else if ui.hovered(&id) {
            theme.hover().patch(theme.text())
        } else {
            theme.muted()
        };
        let text = format!(" {glyph} {label} ");
        let width = u16::try_from(Line::raw(text.as_str()).width()).unwrap_or(0);
        if x + width + 3 > area.right() {
            break;
        }
        ui.line(
            Rect::new(x, area.y, width, 1),
            &Line::from_spans(vec![
                span(format!(" {glyph} "), base.patch(tone)),
                span(format!("{label} "), base),
            ]),
        );
        ui.hit(id.clone(), Rect::new(x, area.y, width, 1));
        x += width;
        if matches!(tab, Tab::Run(_)) {
            x += ui.button(&format!("{id}.close"), x, area.y, "✕ ", theme.faint());
        }
        x += 1;
    }
}

/// `area` in two along a draggable divider: `name` is `col.…` (side by side) or `row.…`
/// (stacked), and the first part takes `default` percent until the divider is dragged.
fn split(app: &App, ui: &mut Ui<'_, '_, '_>, name: &str, area: Rect, default: u16) -> [Rect; 2] {
    let percent = app
        .splits
        .get(name)
        .copied()
        .unwrap_or(default)
        .clamp(10, 90);
    let id = format!("split.{name}");
    ui.areas.set(id.clone(), area);
    if name.starts_with("col.") {
        let first = area.width * percent / 100;
        let handle = Rect::new(area.x + first.saturating_sub(1), area.y, 2, area.height);
        ui.cx.push_hit(
            bmux_tui::hit::HitRegion::new(id, handle)
                .role(HitRole::DragHandle)
                .hoverable(true)
                .layer(2),
        );
        [
            Rect::new(area.x, area.y, first, area.height),
            Rect::new(area.x + first, area.y, area.width - first, area.height),
        ]
    } else {
        let first = area.height * percent / 100;
        let handle = Rect::new(area.x, area.y + first.saturating_sub(1), area.width, 2);
        ui.cx.push_hit(
            bmux_tui::hit::HitRegion::new(id, handle)
                .role(HitRole::DragHandle)
                .hoverable(true)
                .layer(2),
        );
        [
            Rect::new(area.x, area.y, area.width, first),
            Rect::new(area.x, area.y + first, area.width, area.height - first),
        ]
    }
}

fn fleet(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let parts = rows(area, &[Size::Fixed(7), Size::Fill(1)]);
    let top = columns(parts[0], &[Size::Fill(3), Size::Fill(2), Size::Fill(2)]);
    throughput(app, ui, top[0]);
    outcomes(app, ui, top[1]);
    coverage(app, ui, top[2]);
    let main = split(app, ui, "col.fleet", parts[1], 60);
    workers(app, ui, main[0]);
    let side = rows(main[1], &[Size::Fill(1), Size::Fill(1)]);
    invariants_exercised(app, ui, side[0]);
    recent_failures(app, ui, side[1]);
}

fn sparkline_row(
    ui: &mut Ui<'_, '_, '_>,
    area: Rect,
    name: &str,
    label: &str,
    samples: &[u64],
    value: &str,
    tone: Style,
) {
    let theme = ui.theme;
    let parts = columns(area, &[Size::Fixed(13), Size::Fill(1), Size::Fixed(10)]);
    ui.line(parts[0], &one(span(format!(" {label}"), theme.muted())));
    let styles = SparklineStyles {
        normal: tone,
        latest: theme.accent().add_modifier(Modifier::BOLD),
        ..SparklineStyles::from(theme.components)
    };
    sparkline(ui, name, samples, styles, parts[1]);
    ui.line(
        parts[2],
        &one(span(format!(" {value}"), tone.add_modifier(Modifier::BOLD))),
    );
}

/// A sparkline of `samples` (a faint dotted track until there are any).
fn sparkline(
    ui: &mut Ui<'_, '_, '_>,
    name: &str,
    samples: &[u64],
    styles: SparklineStyles,
    area: Rect,
) {
    if samples.is_empty() {
        let theme = ui.theme;
        ui.line(
            area,
            &one(span("·".repeat(usize::from(area.width)), theme.faint())),
        );
        return;
    }
    let line = SparklineComponent::new(name.to_owned(), samples).styles(styles);
    ui.component(name, &line, area);
}

fn throughput(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let elapsed = app.now.duration_since(app.store.started).as_secs();
    let inner = ui.pane(
        "pane.throughput",
        one(span(" Throughput ", theme.strong())),
        Some(one(span(
            format!(
                " {:02}:{:02}:{:02} ",
                elapsed / 3600,
                elapsed / 60 % 60,
                elapsed % 60
            ),
            theme.muted(),
        ))),
        area,
        false,
    );
    let lines = rows(
        inner,
        &[
            Size::Fixed(1),
            Size::Fixed(1),
            Size::Fixed(1),
            Size::Fill(1),
        ],
    );
    let store = &app.store;
    sparkline_row(
        ui,
        lines[0],
        "spark.runs",
        "runs/s",
        &store.runs_per_second.samples(),
        &human(store.runs_per_second.last().unwrap_or(0)),
        theme.info(),
    );
    sparkline_row(
        ui,
        lines[1],
        "spark.decisions",
        "decisions/s",
        &store.decisions_per_second.samples(),
        &human(store.decisions_per_second.last().unwrap_or(0)),
        theme.good(),
    );
    let speed = store.speedup.last().unwrap_or(0);
    sparkline_row(
        ui,
        lines[2],
        "spark.speed",
        "sim speed",
        &store.speedup.samples(),
        &format!("×{}", human(speed / 100)),
        theme.replay(),
    );
    let runs = store.fleet.and_then(|f| f.runs).map_or_else(
        || "runs until stopped".to_owned(),
        |runs| format!("{} of {} runs", grouped(store.completed()), grouped(runs)),
    );
    ui.line(lines[3], &one(span(format!(" {runs}"), theme.muted())));
}

fn outcomes(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let inner = ui.pane(
        "pane.outcomes",
        one(span(" Outcomes ", theme.strong())),
        app.filter
            .as_ref()
            .map(|f| one(span(format!(" filter {f} "), theme.accent()))),
        area,
        app.filter.is_some(),
    );
    let totals = &app.store.totals;
    ui.line(
        row(inner, 0),
        &Line::from_spans(vec![
            span(" ✔ ", theme.good()),
            span(
                grouped(totals.passed),
                theme.good().add_modifier(Modifier::BOLD),
            ),
            span(" passed  ", theme.muted()),
            span(
                "✖ ",
                if totals.failed > 0 {
                    theme.bad()
                } else {
                    theme.muted()
                },
            ),
            span(
                grouped(totals.failed),
                if totals.failed > 0 {
                    theme.bad().add_modifier(Modifier::BOLD)
                } else {
                    theme.muted()
                },
            ),
            span(" failed", theme.muted()),
        ]),
    );
    let max = totals
        .by_tag
        .iter()
        .filter(|(tag, _)| tag.starts_with("personality:"))
        .map(|(_, (p, f))| p + f)
        .max()
        .unwrap_or(1)
        .max(1);
    for (y, (tag, (passed, failed))) in (1_u16..).zip(
        totals
            .by_tag
            .iter()
            .filter(|(tag, _)| tag.starts_with("personality:")),
    ) {
        if y >= inner.height {
            break;
        }
        let line_area = row(inner, y);
        let id = format!("filter.{tag}");
        let active = app.filter.as_deref() == Some(tag.as_str());
        if ui.hovered(&id) || active {
            ui.fill(line_area, theme.hover());
        }
        #[allow(clippy::cast_precision_loss)] // display
        let fraction = (passed + failed) as f64 / max as f64;
        let bar_width = usize::from(inner.width.saturating_sub(26));
        let value = tag.split_once(':').map_or(tag.as_str(), |(_, v)| v);
        ui.line(
            line_area,
            &Line::from_spans(vec![
                span(" ", theme.text()),
                chip(theme, tag),
                span(" ", theme.text()),
                span(
                    format!("{:<width$}", bar(fraction, bar_width), width = bar_width),
                    theme.fg(theme.personality(value)),
                ),
                span(format!(" {:>7}", human(*passed)), theme.good()),
                span(
                    if *failed > 0 {
                        format!(" ✖{failed}")
                    } else {
                        String::new()
                    },
                    theme.bad(),
                ),
            ]),
        );
        ui.hit(id, line_area);
    }
}

fn coverage(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let inner = ui.pane(
        "pane.coverage",
        one(span(" Coverage ", theme.strong())),
        None,
        area,
        false,
    );
    let store = &app.store;
    #[allow(clippy::cast_precision_loss)] // display
    let novelty = if store.completed() == 0 {
        0.0
    } else {
        store.distinct_traces as f64 / store.completed() as f64 * 100.0
    };
    ui.line(
        row(inner, 0),
        &Line::from_spans(vec![
            span(" ", theme.text()),
            span(
                grouped(store.distinct_traces),
                theme.info().add_modifier(Modifier::BOLD),
            ),
            span(" distinct schedules", theme.muted()),
            span(
                format!(" ({novelty:.0}% new)"),
                if novelty < 50.0 {
                    theme.warn()
                } else {
                    theme.muted()
                },
            ),
        ]),
    );
    let mut shapes: Vec<(&String, &u64)> = store
        .shapes
        .iter()
        .filter(|(tag, _)| !tag.starts_with("personality:"))
        .collect();
    shapes.sort_by(|a, b| b.1.cmp(a.1));
    let max = shapes.first().map_or(1, |(_, n)| **n).max(1);
    for (index, (tag, count)) in shapes.into_iter().enumerate() {
        let Ok(y) = u16::try_from(index + 1) else {
            break;
        };
        if y >= inner.height {
            break;
        }
        #[allow(clippy::cast_precision_loss)] // display
        let fraction = *count as f64 / max as f64;
        let width = usize::from(inner.width.saturating_sub(24));
        let id = format!("filter.{tag}");
        let line_area = row(inner, y);
        if ui.hovered(&id) || app.filter.as_deref() == Some(tag.as_str()) {
            ui.fill(line_area, theme.hover());
        }
        ui.line(
            line_area,
            &Line::from_spans(vec![
                span(format!(" {:<12}", truncate(tag, 12)), theme.text()),
                span(format!("{:<width$}", bar(fraction, width)), theme.info()),
                span(format!(" {:>7}", human(*count)), theme.muted()),
            ]),
        );
        ui.hit(id, line_area);
    }
}

fn worker_row(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect, run: &RunRecord, selected: bool) {
    let theme = ui.theme;
    let id = format!("worker.{}.{}", run.id.worker, run.id.run);
    if ui.hovered(&id) {
        ui.fill(area, theme.hover());
    } else if selected {
        ui.fill(area, theme.raised());
    }
    let stats = app
        .store
        .workers
        .get(&run.worker)
        .cloned()
        .unwrap_or_default();
    let parts = columns(
        area,
        &[
            Size::Fixed(5),
            Size::Fill(1),
            Size::Fixed(14),
            Size::Fixed(9),
            Size::Fixed(10),
            Size::Fixed(11),
        ],
    );
    ui.line(
        parts[0],
        &Line::from_spans(vec![
            span(if selected { "▌" } else { " " }, theme.accent()),
            span(format!("w{:<2}", run.worker), theme.muted()),
        ]),
    );
    // The worker's latest outcomes, newest at the right: one cell per run.
    let width = usize::from(parts[1].width.saturating_sub(2));
    let skip = stats.recent.len().saturating_sub(width);
    let mut strip = vec![span("▕", theme.faint())];
    for (index, passed) in stats.recent.iter().skip(skip).enumerate() {
        let newest = index + 1 == stats.recent.len() - skip;
        let style = if *passed {
            theme.good()
        } else {
            theme.bad().add_modifier(Modifier::BOLD)
        };
        let glyph = if *passed { "▮" } else { "✖" };
        strip.push(span(
            glyph,
            if newest {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            },
        ));
    }
    let running = if ui.tick % 6 < 3 { "▯" } else { "▮" };
    strip.push(span(running, theme.faint()));
    ui.line(parts[1], &Line::from_spans(strip));
    ui.line(
        parts[2],
        &Line::from_spans(vec![
            span(format!(" {:>6} runs", human(stats.runs)), theme.text()),
            span(
                if stats.failed > 0 {
                    format!(" ✖{}", stats.failed)
                } else {
                    String::new()
                },
                theme.bad(),
            ),
        ]),
    );
    ui.line(
        parts[3],
        &one(span(format!(" #{:<7}", run.id.run), theme.muted())),
    );
    ui.line(
        parts[4],
        &one(span(format!(" {} ", short_seed(run.seed)), theme.info())),
    );
    if let Some(personality) = run.tag("personality") {
        ui.line(
            parts[5],
            &one(chip(theme, &format!("personality:{personality}"))),
        );
    }
    ui.hit(id, area);
}

fn workers(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let visible = app.visible_workers();
    let right = one(span(format!(" {} workers ", visible.len()), theme.good()));
    let inner = ui.pane(
        "pane.workers",
        one(span(" Workers ", theme.strong())),
        Some(right),
        area,
        true,
    );
    ui.hit_as("list.workers", inner, HitRole::Scroll);
    if visible.is_empty() {
        let text = if app.store.finished {
            "Every run is done."
        } else if app.controller.as_ref().is_some_and(|c| c.status().paused) {
            "The fleet is paused: press p (or ❚❚) to resume."
        } else {
            "Waiting for runs to start…"
        };
        ui.line(row(inset(inner, 1), 0), &one(span(text, theme.muted())));
        return;
    }
    let offset = usize::from(app.scroll.get("workers").copied().unwrap_or(0))
        .min(visible.len().saturating_sub(usize::from(inner.height)));
    for (index, run) in visible.iter().enumerate().skip(offset) {
        let Ok(y) = u16::try_from(index - offset) else {
            break;
        };
        if y >= inner.height {
            break;
        }
        worker_row(app, ui, row(inner, y), run, index == app.selected_worker);
    }
}

fn invariants_exercised(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let inner = ui.pane(
        "pane.invariants",
        one(span(" Invariants exercised ", theme.strong())),
        None,
        area,
        false,
    );
    let invariants = &app.store.invariants;
    if invariants.is_empty() {
        ui.line(
            row(inner, 0),
            &one(span(" none reported yet", theme.muted())),
        );
        return;
    }
    let max = invariants
        .values()
        .map(|t| t.checked)
        .max()
        .unwrap_or(1)
        .max(1);
    for (index, (name, total)) in invariants.iter().enumerate() {
        let Ok(y) = u16::try_from(index) else { break };
        if y >= inner.height {
            break;
        }
        #[allow(clippy::cast_precision_loss)] // display
        let fraction = total.checked as f64 / max as f64;
        let name_width = usize::from(inner.width / 2).clamp(18, 48);
        let width = usize::from(inner.width).saturating_sub(name_width + 16);
        let (tone, flag) = if total.failed > 0 {
            (theme.bad(), format!(" ✖{}", total.failed))
        } else if total.checked == 0 {
            (theme.warn(), " ⚠ never".into())
        } else if fraction < 0.01 {
            (theme.warn(), " ⚠ rare".into())
        } else {
            (theme.good(), String::new())
        };
        ui.line(
            row(inner, y),
            &Line::from_spans(vec![
                span(
                    format!(" {:<name_width$}", truncate(name, name_width)),
                    theme.text(),
                ),
                span(format!("{:<width$}", bar(fraction, width)), tone),
                span(format!(" {:>7}", human(total.checked)), theme.muted()),
                span(flag, tone),
            ]),
        );
    }
}

/// A failure group's status glyphs: reproduction and shrinking.
fn failure_status(theme: &Theme, group: &crate::store::FailureGroup, tick: u64) -> Vec<Span> {
    use crate::store::Verdict;
    let mut out = Vec::new();
    let verdict = group.verdict();
    let (text, style) = match verdict {
        Verdict::Replaying => (format!(" {} replaying", spinner(tick)), theme.info()),
        Verdict::Reproduced => (" ↺✓ reproduced".into(), theme.good()),
        Verdict::Nondeterministic => (" ↺✗ nondeterministic".into(), theme.warn()),
        Verdict::EarlierBuild => (" ◷ earlier build".into(), theme.muted()),
        Verdict::StillFails => (" ↺✓ still fails".into(), theme.bad()),
        Verdict::PassesNow => (" ✔ passes now".into(), theme.good()),
        Verdict::Unconfirmed => (String::new(), theme.muted()),
    };
    if !text.is_empty() {
        out.push(span(text, style));
    }
    if let Some(shrink) = &group.shrink {
        if shrink.done && shrink.best == 0 {
            out.push(span(" ⇲ any schedule", theme.good()));
        } else if shrink.done {
            out.push(span(
                format!(" ⇲ {} choices", grouped(shrink.best)),
                theme.good(),
            ));
        } else {
            out.push(span(
                format!(" {} shrinking", spinner(tick)),
                theme.replay(),
            ));
        }
    }
    out
}

fn recent_failures(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let count = app.store.failures.len();
    let inner = ui.pane(
        "pane.recent",
        one(span(
            " Failures ",
            if count > 0 {
                theme.bad().add_modifier(Modifier::BOLD)
            } else {
                theme.strong()
            },
        )),
        Some(one(span(" f to triage ", theme.muted()))),
        area,
        count > 0,
    );
    if count == 0 {
        ui.line(row(inner, 0), &one(span(" ✔ none so far", theme.good())));
        return;
    }
    let mut ordered: Vec<(usize, &crate::store::FailureGroup)> =
        app.store.failures.iter().enumerate().collect();
    ordered.sort_by_key(|(_, g)| std::cmp::Reverse(g.last_seen));
    for (y, (index, group)) in ordered.into_iter().enumerate() {
        let Ok(y) = u16::try_from(y) else { break };
        if y >= inner.height {
            break;
        }
        let id = format!("recent.{index}");
        let line_area = row(inner, y);
        if ui.hovered(&id) {
            ui.fill(line_area, theme.hover());
        }
        let fresh =
            app.now.duration_since(group.last_seen) < Duration::from_secs(3) && ui.tick % 4 < 2;
        let mut spans = vec![
            span(
                " ✖",
                if fresh {
                    theme.bad().add_modifier(Modifier::REVERSED)
                } else {
                    theme.bad()
                },
            ),
            span(format!(" ×{:<4}", group.count), theme.muted()),
            span(
                truncate(
                    &group.signature,
                    usize::from(inner.width.saturating_sub(30)),
                ),
                theme.text(),
            ),
        ];
        spans.extend(failure_status(theme, group, ui.tick));
        ui.line(line_area, &Line::from_spans(spans));
        ui.hit(id, line_area);
    }
}

fn failures(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let parts = columns(area, &[Size::Fill(2), Size::Fill(3)]);
    let groups = &app.store.failures;
    let inner = ui.pane(
        "pane.failures",
        one(span(" Failures by cause ", theme.strong())),
        Some(one(span(format!(" {} ", groups.len()), theme.muted()))),
        parts[0],
        true,
    );
    ui.hit_as("list.failures", inner, HitRole::Scroll);
    if groups.is_empty() {
        ui.line(
            row(inset(inner, 1), 0),
            &one(span(
                "✔ No failures. Failing runs land here, grouped, reproduced and shrunk.",
                theme.good(),
            )),
        );
    }
    let offset = usize::from(app.scroll.get("failures").copied().unwrap_or(0));
    for (index, group) in groups.iter().enumerate().skip(offset) {
        let Ok(y) = u16::try_from((index - offset) * 2) else {
            break;
        };
        if y + 1 >= inner.height {
            break;
        }
        let id = format!("failure.{index}");
        let block = Rect::new(inner.x, inner.y + y, inner.width, 2);
        let selected = index == app.selected_failure;
        if selected {
            ui.fill(block, theme.raised());
        } else if ui.hovered(&id) {
            ui.fill(block, theme.hover());
        }
        ui.line(
            row(block, 0),
            &Line::from_spans(vec![
                span(if selected { "▌" } else { " " }, theme.accent()),
                span("✖ ", theme.bad()),
                span(
                    truncate(&group.signature, usize::from(inner.width.saturating_sub(4))),
                    theme.text().add_modifier(Modifier::BOLD),
                ),
            ]),
        );
        let mut detail = vec![
            span(format!("   ×{} ", group.count), theme.muted()),
            span(format!("seed {} ", short_seed(group.seed)), theme.info()),
        ];
        for tag in group.shape.iter().filter(|t| t.starts_with("personality:")) {
            detail.push(chip(theme, tag));
        }
        detail.extend(failure_status(theme, group, ui.tick));
        ui.line(row(block, 1), &Line::from_spans(detail));
        ui.hit(id, block);
    }
    failure_card(app, ui, parts[1]);
}

#[allow(clippy::too_many_lines)] // the card's sections are drawn together
fn failure_card(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let shown = app.selected_failure;
    let Some(group) = app.store.failures.get(shown) else {
        let inner = ui.pane(
            "pane.failure",
            one(span(" Failure ", theme.strong())),
            None,
            area,
            false,
        );
        ui.line(
            row(inner, 1),
            &one(span("  Select a failure to see it here.", theme.muted())),
        );
        return;
    };
    let inner = ui.pane(
        "pane.failure",
        one(span(
            format!(
                " {} ",
                truncate(&group.signature, usize::from(area.width.saturating_sub(6)))
            ),
            theme.bad().add_modifier(Modifier::BOLD),
        )),
        None,
        area,
        true,
    );
    let parts = rows(
        inner,
        &[
            Size::Fixed(5),
            Size::Fixed(2),
            Size::Fill(1),
            Size::Fixed(2),
            Size::Fixed(1),
        ],
    );
    let mut facts = vec![
        Line::from_spans(vec![
            span(" runs  ", theme.muted()),
            span(format!("{} failed this way", group.count), theme.text()),
            span(
                format!(
                    "  first {} · last {}",
                    ago(app.now.duration_since(group.first_seen)),
                    ago(app.now.duration_since(group.last_seen))
                ),
                theme.muted(),
            ),
        ]),
        Line::from_spans(vec![
            span(" seed  ", theme.muted()),
            span(format!("{}", group.seed), theme.info()),
            span(format!("  epoch {}", group.epoch_offset), theme.muted()),
        ]),
    ];
    let mut shape = vec![span(" shape ", theme.muted())];
    for tag in &group.shape {
        shape.push(chip(theme, tag));
        shape.push(span(" ", theme.text()));
    }
    facts.push(Line::from_spans(shape));
    facts.push(Line::from_spans(vec![
        span(" state ", theme.muted()),
        span(
            group.verdict().describe(),
            theme.text().add_modifier(Modifier::BOLD),
        ),
    ]));
    if group.stale {
        facts.push(one(span(
            " saved by another build: its seed may run differently on this one (↺ checks it, ✕ forgets it)",
            theme.muted(),
        )));
    }
    ui.lines(parts[0], &facts);
    if let Some(shrink) = &group.shrink {
        let value = if shrink.done || shrink.original == 0 {
            ProgressBarValue::ratio(1, 1)
        } else {
            ProgressBarValue::ratio(
                shrink.original - shrink.best.min(shrink.original),
                shrink.original,
            )
        };
        let label = row(parts[1], 0);
        ui.line(
            label,
            &Line::from_spans(vec![
                span(" shrinking ", theme.muted()),
                span(
                    format!(
                        "{} → {} choices after {} replays",
                        grouped(shrink.original),
                        grouped(shrink.best),
                        grouped(shrink.attempts)
                    ),
                    theme.replay(),
                ),
            ]),
        );
        let progress =
            ProgressBarComponent::new("failure.shrink.progress", value).styles(ProgressBarStyles {
                filled: theme.replay(),
                complete: theme.good(),
                ..ProgressBarStyles::from(theme.components)
            });
        ui.component(
            "failure.shrink.progress",
            &progress,
            inset(row(parts[1], 1), 0),
        );
    }
    // What failed, then the few facts of the harness's report that matter.
    let report: Vec<Line> = failure_lines(&group.report)
        .into_iter()
        .map(|line| one(span(format!(" {line}"), theme.bad())))
        .chain(std::iter::once(Line::raw("")))
        .chain(std::iter::once(one(span(
            format!(" {}", run_facts(&group.report)),
            theme.muted(),
        ))))
        .collect();
    ui.hit_as("list.report", parts[2], HitRole::Scroll);
    let offset = usize::from(app.scroll.get("report").copied().unwrap_or(0))
        .min(report.len().saturating_sub(1));
    ui.fill(parts[2], theme.canvas());
    ui.lines(parts[2], &report[offset..]);
    let command = group.replay_command().unwrap_or_else(|| {
        format!(
            "SIMULATOR_SEED={} SIMULATOR_EPOCH_OFFSET={} SIMULATOR_RUNS=1",
            group.seed, group.epoch_offset
        )
    });
    ui.lines(
        parts[3],
        &[
            one(span(" replay from a shell:", theme.muted())),
            one(span(format!(" {command}"), theme.info())),
        ],
    );
    let mut x = parts[4].x + 1;
    let y = parts[4].y;
    x += ui.button(
        &format!("failure.{shown}.open"),
        x,
        y,
        " ▶ Open and debug ",
        theme.accent().add_modifier(Modifier::BOLD),
    ) + 1;
    x += ui.button(
        &format!("failure.{shown}.confirm"),
        x,
        y,
        " ↺ Reproduce ",
        theme.info(),
    ) + 1;
    x += ui.button(
        &format!("failure.{shown}.shrink"),
        x,
        y,
        " ⇲ Shrink ",
        theme.replay(),
    ) + 1;
    ui.button(
        &format!("failure.{shown}.forget"),
        x,
        y,
        " ✕ Forget ",
        theme.muted(),
    );
}

/// The lines of a report that say what failed: after `Error:`, up to the harness's own
/// summary.
fn failure_lines(report: &str) -> Vec<String> {
    report
        .split_once("Error:")
        .map_or(report, |(_, rest)| rest)
        .lines()
        .map(str::trim_end)
        .take_while(|line| !line.starts_with('=') && !line.starts_with("To run"))
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// The facts of a harness report worth a glance: run, steps, real and simulated time.
fn run_facts(report: &str) -> String {
    let field = |name: &str| {
        report
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(str::trim)
    };
    [
        field("run").map(|v| format!("run {v}")),
        field("steps").map(|v| format!("{v} steps")),
        field("real_time_elapsed").map(|v| format!("{v} real")),
        field("simulated_time_elapsed").map(|v| format!("{v} simulated")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The scheduler of the run shown, and its timeline index.
fn run_data(
    app: &App,
) -> (
    Option<&RunRecord>,
    Option<&SchedulerView>,
    Option<std::sync::Arc<RunIndex>>,
) {
    let Some(tab) = app.current_run() else {
        return (None, None, None);
    };
    let origin = app.store.runs.get(&tab.origin);
    let shown = app.store.runs.get(&tab.shown());
    let scheduler = shown
        .and_then(|r| r.scheduler.as_ref())
        .or_else(|| origin.and_then(|r| r.scheduler.as_ref()));
    (origin, scheduler, app.index(tab.origin))
}

#[allow(clippy::too_many_lines)] // the run's header, pages and page are drawn together
fn run(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let Some(tab) = app.current_run() else { return };
    let theme = ui.theme;
    let parts = rows(area, &[Size::Fixed(2), Size::Fixed(1), Size::Fill(1)]);
    let (origin, _, _) = run_data(app);
    // Who the run is, and where it stands.
    ui.fill(parts[0], theme.surface());
    let (status, style) = if tab.travel.is_some() {
        ("◆ REPLAY", theme.replay())
    } else if origin.is_some_and(RunRecord::failed) {
        ("✖ FAILED", theme.bad())
    } else if origin.is_some_and(RunRecord::live) {
        (
            if ui.tick % 10 < 5 {
                "● LIVE"
            } else {
                "○ LIVE"
            },
            theme.good(),
        )
    } else {
        ("✔ PASSED", theme.good())
    };
    let mut first = vec![
        span(
            format!(" {status} "),
            style.add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        span(format!(" seed {} ", tab.seed), theme.info()),
        span(format!("epoch {} ", tab.epoch_offset), theme.muted()),
    ];
    if let Some(origin) = origin {
        if origin.kind == RunKind::Fleet {
            first.push(span(
                format!("· worker {} run {} ", origin.worker, origin.id.run),
                theme.muted(),
            ));
        }
        for tag in &origin.shape {
            first.push(chip(theme, tag));
            first.push(span(" ", theme.text()));
        }
    }
    ui.line(row(parts[0], 0), &Line::from_spans(first));
    let mut second = Vec::new();
    if let Some((position, length)) = app.position() {
        second.push(span(" decision ", theme.muted()));
        second.push(span(grouped(position), theme.strong()));
        if let Some(length) = length {
            second.push(span(format!(" of {}", grouped(length)), theme.muted()));
        }
    }
    if let Some(travel) = &tab.travel {
        let parked = travel.keyframes.iter().filter(|k| k.parked()).count();
        second.push(span(
            format!("   keyframes {parked}/{}", travel.keyframes.len()),
            theme.replay(),
        ));
        if !travel.settled() {
            second.push(span(
                format!(" {} seeking", spinner(ui.tick)),
                theme.replay(),
            ));
        }
        let breaks = travel.cursor.control.breakpoints();
        if !breaks.is_empty() {
            second.push(span(
                format!(
                    "   ◆ {} breakpoint{}",
                    breaks.len(),
                    if breaks.len() == 1 { "" } else { "s" }
                ),
                theme.bad(),
            ));
        }
        if let Some(hit) = travel.cursor.control.hit() {
            second.push(span(
                format!("   stopped: {hit}"),
                theme.warn().add_modifier(Modifier::BOLD),
            ));
        }
    }
    if let Some(outcome) = origin.and_then(|o| o.outcome.as_ref()) {
        second.push(span(
            format!(
                "   {:.1}s simulated in {:.2}s",
                outcome.sim_time.as_secs_f64(),
                outcome.real_time.as_secs_f64()
            ),
            theme.muted(),
        ));
    }
    ui.line(row(parts[0], 1), &Line::from_spans(second));
    // Pages.
    let mut x = parts[1].x + 1;
    let mut pages: Vec<(Page, String)> = vec![
        (Page::Overview, "Overview".into()),
        (Page::Threads, "Threads".into()),
        (Page::Invariants, "Invariants".into()),
        (Page::Decisions, "Decisions".into()),
    ];
    for (index, view) in app.views.iter().enumerate() {
        pages.push((Page::View(index), view.title()));
    }
    for (page, title) in pages {
        let id = format!("page.{}", page.id());
        let style = if page == tab.page {
            theme
                .accent()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINE)
        } else {
            theme.muted()
        };
        x += ui.button(&id, x, parts[1].y, &format!(" {title} "), style) + 1;
    }
    match tab.page {
        Page::Overview => overview(app, ui, parts[2]),
        Page::Threads => threads(app, ui, parts[2]),
        Page::Invariants => invariants_page(app, ui, parts[2]),
        Page::Decisions => decisions(app, ui, parts[2]),
        Page::View(index) => sim_view(app, ui, parts[2], index),
    }
}

fn overview(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let parts = split(app, ui, "row.overview", area, 60);
    timeline(app, ui, parts[0]);
    let bottom = columns(parts[1], &[Size::Fill(1), Size::Fill(1)]);
    invariants_summary(app, ui, bottom[0]);
    events_near(app, ui, bottom[1]);
}

/// Thread `index`'s color.
fn thread_style(theme: &Theme, index: usize) -> Style {
    theme.fg(theme.thread(index))
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)] // drawn together; display math
fn timeline(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let (_, scheduler, index) = run_data(app);
    let range = app.timeline_range();
    let right = range.map(|(lo, hi)| {
        one(span(
            format!(" {} – {} ", grouped(lo), grouped(hi)),
            theme.muted(),
        ))
    });
    let inner = ui.pane(
        "pane.timeline",
        one(span(" Timeline ", theme.strong())),
        right,
        area,
        true,
    );
    let Some(index) = index else {
        ui.line(
            row(inset(inner, 1), 0),
            &one(span(
                "The timeline appears as the run reports it.",
                theme.muted(),
            )),
        );
        return;
    };
    let Some((lo, hi)) = range else { return };
    let gutter = 16_u16.min(inner.width / 3);
    let track = Rect::new(
        inner.x + gutter,
        inner.y,
        inner.width.saturating_sub(gutter),
        inner.height,
    );
    if track.width == 0 || track.height < 3 {
        return;
    }
    // The ruler.
    let mut ruler = String::new();
    let step = (track.width / 4).max(1);
    let mut column = 0;
    while column < track.width {
        let decision = lo + (hi - lo) * u64::from(column) / u64::from(track.width);
        let label = format!("┊{}", human(decision));
        let width = u16::try_from(label.chars().count()).unwrap_or(0);
        if column + width > track.width {
            break;
        }
        ruler.push_str(&label);
        ruler.push_str(&" ".repeat(usize::from(step.saturating_sub(width))));
        column += step.max(width);
    }
    ui.line(row(track, 0), &one(span(ruler, theme.faint())));
    // Lanes: the busiest threads first, as many as fit.
    let lanes_height = track.height.saturating_sub(2);
    let mut order: Vec<usize> = (0..index.threads.len()).collect();
    order.sort_by_key(|&t| {
        std::cmp::Reverse(index.activity[t].iter().map(|n| u64::from(*n)).sum::<u64>())
    });
    order.truncate(usize::from(lanes_height));
    order.sort_unstable();
    let columns_count = usize::from(track.width);
    let span_per_column = (hi - lo).max(1) as f64 / columns_count as f64;
    let max_per_column = (span_per_column / index.bucket as f64).max(1.0) * index.bucket as f64;
    for (lane, &thread) in order.iter().enumerate() {
        let Ok(y) = u16::try_from(lane + 1) else {
            break;
        };
        let name = &index.threads[thread];
        let state = scheduler
            .and_then(|s| s.threads.iter().find(|t| &t.name == name))
            .map_or("", |t| t.state.as_str());
        let label_area = Rect::new(inner.x, track.y + y, gutter, 1);
        let id = format!("lane.{thread}");
        if ui.hovered(&id) {
            ui.fill(label_area, theme.hover());
        }
        ui.line(
            label_area,
            &Line::from_spans(vec![
                span(
                    format!(" {}", Theme::thread_glyph(state)),
                    theme.thread_state(state),
                ),
                span(
                    format!(" {}", truncate(name, usize::from(gutter.saturating_sub(4)))),
                    thread_style(theme, thread),
                ),
            ]),
        );
        ui.hit(id, label_area);
        let hue = theme.thread(thread);
        let lane_data = &index.activity[thread];
        for column in 0..columns_count {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )] // display
            let (start, end) = (
                lo + (span_per_column * column as f64) as u64,
                lo + (span_per_column * (column + 1) as f64).ceil() as u64,
            );
            let first = usize::try_from(start / index.bucket).unwrap_or(usize::MAX);
            let last = usize::try_from(end.saturating_sub(1) / index.bucket).unwrap_or(usize::MAX);
            let count: u64 = lane_data
                .get(first..=last.min(lane_data.len().saturating_sub(1)))
                .unwrap_or(&[])
                .iter()
                .map(|n| u64::from(*n))
                .sum();
            #[allow(clippy::cast_precision_loss)] // display
            let heat = (count as f64 / max_per_column).clamp(0.0, 1.0);
            let x = track.x + u16::try_from(column).unwrap_or(0);
            if heat == 0.0 {
                ui.cell(x, track.y + y, "·", theme.faint());
            } else {
                const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0..8
                let level = ((heat * 7.0).round() as usize).min(7);
                let color = Theme::blend(theme.palette.faint, hue, heat.mul_add(0.65, 0.35));
                ui.cell(x, track.y + y, LEVELS[level], theme.fg(color));
            }
        }
    }
    // Events.
    let events_y = track.y + track.height - 1;
    ui.line(
        Rect::new(inner.x, events_y, gutter, 1),
        &one(span(" events", theme.muted())),
    );
    for event in index
        .events
        .iter()
        .filter(|e| e.decision >= lo && e.decision < hi)
    {
        let column =
            u16::try_from((event.decision - lo) * u64::from(track.width) / (hi - lo).max(1))
                .unwrap_or(0);
        let x = track.x + column.min(track.width - 1);
        let glyph = match event.tone {
            Tone::Bad => "✖",
            Tone::Warn => "▲",
            Tone::Good => "✔",
            Tone::Info => "◆",
            Tone::Neutral => "•",
        };
        let id = format!("event.{}", event.decision);
        let style = if ui.hovered(&id) {
            theme.tone(event.tone).add_modifier(Modifier::REVERSED)
        } else {
            theme.tone(event.tone)
        };
        ui.cell(x, events_y, glyph, style);
        ui.cx.push_hit(
            bmux_tui::hit::HitRegion::new(id, Rect::new(x, events_y, 1, 1))
                .hoverable(true)
                .layer(3),
        );
    }
    // Breakpoints and the cursor.
    let travel = app.current_run().and_then(|r| r.travel.as_ref());
    if let Some(travel) = travel {
        for breakpoint in travel.cursor.control.breakpoints() {
            if let simvar_observer::Breakpoint::Decision(decision) = breakpoint
                && decision >= lo
                && decision < hi
            {
                let column =
                    u16::try_from((decision - lo) * u64::from(track.width) / (hi - lo).max(1))
                        .unwrap_or(0);
                ui.cell(
                    track.x + column.min(track.width - 1),
                    track.y,
                    "◆",
                    theme.bad(),
                );
            }
        }
    }
    if let Some((position, _)) = app.position()
        && position >= lo
        && position < hi
    {
        let column =
            u16::try_from((position - lo) * u64::from(track.width) / (hi - lo).max(1)).unwrap_or(0);
        let x = track.x + column.min(track.width - 1);
        let style = if travel.is_some() {
            theme.replay()
        } else {
            theme.good()
        }
        .add_modifier(Modifier::BOLD);
        ui.cell(x, track.y, "▼", style);
        for y in 1..track.height - 1 {
            ui.cell(x, track.y + y, "│", style);
        }
    }
    // Hovering the track shows the decision under the pointer.
    ui.areas.set("timeline", track);
    ui.hit_as(
        "timeline",
        Rect::new(track.x, track.y, track.width, track.height - 1),
        HitRole::DragHandle,
    );
}

fn invariants_summary(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let (origin, _, _) = run_data(app);
    let shown = app
        .current_run()
        .and_then(|t| app.store.runs.get(&t.shown()))
        .and_then(|r| r.invariants.as_ref());
    let invariants = shown.or_else(|| origin.and_then(|o| o.invariants.as_ref()));
    let inner = ui.pane(
        "pane.run.invariants",
        one(span(" Invariants ", theme.strong())),
        None,
        area,
        false,
    );
    let Some(invariants) = invariants else {
        ui.line(row(inner, 0), &one(span(" not reported", theme.muted())));
        return;
    };
    let mut lines = Vec::new();
    for status in &invariants.0 {
        let (glyph, style) = if status.failures.is_empty() {
            if status.checked == 0 {
                ("○", theme.muted())
            } else {
                ("✔", theme.good())
            }
        } else {
            ("✖", theme.bad())
        };
        lines.push(Line::from_spans(vec![
            span(format!(" {glyph} "), style),
            span(
                truncate(&status.name, usize::from(inner.width.saturating_sub(14))),
                theme.text(),
            ),
            span(format!(" ×{}", human(status.checked)), theme.muted()),
        ]));
        for failure in &status.failures {
            lines.push(one(span(
                format!(
                    "     {}",
                    truncate(failure, usize::from(inner.width.saturating_sub(6)))
                ),
                theme.bad(),
            )));
        }
    }
    ui.lines(inner, &lines);
}

/// When the run shown failed (or was opened for a failure), what failed and where the replay
/// stopped, at the top of `area`; returns what is left of it.
fn why_it_failed(
    app: &App,
    ui: &mut Ui<'_, '_, '_>,
    area: Rect,
    origin: Option<&RunRecord>,
) -> Rect {
    let theme = ui.theme;
    let tab = app.current_run();
    let report = origin
        .and_then(|o| o.outcome.as_ref())
        .filter(|o| !o.success)
        .map(|o| o.report.clone())
        .or_else(|| {
            let signature = tab?.failure.as_ref()?;
            app.store
                .failures
                .iter()
                .find(|g| &g.signature == signature)
                .map(|g| g.report.clone())
        });
    if let Some(report) = report {
        let mut lines: Vec<Line> = failure_lines(&report)
            .into_iter()
            .map(|line| one(span(format!(" {line}"), theme.bad())))
            .collect();
        let travel = tab.and_then(|t| t.travel.as_ref());
        match travel.and_then(|t| t.cursor.control.hit()) {
            Some(hit) => lines.push(one(span(
                format!(" ◆ the replay stopped here: {hit}"),
                theme.warn(),
            ))),
            None if travel.is_some_and(|t| !t.settled()) => lines.push(one(span(
                format!(" {} replaying to the failure…", spinner(ui.tick)),
                theme.replay(),
            ))),
            None if travel
                .and_then(|t| t.cursor.run)
                .and_then(|id| app.store.runs.get(&id))
                .and_then(|r| r.outcome.as_ref())
                .is_some_and(|o| o.success) =>
            {
                lines.push(one(span(
                    " ✔ this replay passed: the failure does not happen on this build",
                    theme.good().add_modifier(Modifier::BOLD),
                )));
            }
            None => {}
        }
        lines.push(one(span(format!(" {}", run_facts(&report)), theme.muted())));
        let height = u16::try_from(lines.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height / 2);
        let parts = rows(area, &[Size::Fixed(height), Size::Fill(1)]);
        let inner = ui.pane(
            "pane.run.failure",
            one(span(
                " Why it failed ",
                theme.bad().add_modifier(Modifier::BOLD),
            )),
            None,
            parts[0],
            true,
        );
        ui.lines(inner, &lines);
        parts[1]
    } else {
        area
    }
}

fn events_near(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let (origin, _, index) = run_data(app);
    let area = why_it_failed(app, ui, area, origin);
    let inner = ui.pane(
        "pane.run.events",
        one(span(" Events ", theme.strong())),
        Some(one(span(" [ ] jump ", theme.muted()))),
        area,
        false,
    );
    let Some(index) = index else { return };
    let position = app.position().map_or(0, |(p, _)| p);
    let at = index
        .events
        .iter()
        .position(|e| e.decision > position)
        .unwrap_or(index.events.len());
    let half = usize::from(inner.height / 2);
    let start = at.saturating_sub(half);
    for (row_index, event) in index.events.iter().skip(start).enumerate() {
        let Ok(y) = u16::try_from(row_index) else {
            break;
        };
        if y >= inner.height {
            break;
        }
        let id = format!("event.{}", event.decision);
        let line_area = row(inner, y);
        let past = event.decision <= position;
        if ui.hovered(&id) {
            ui.fill(line_area, theme.hover());
        }
        let marker = if start + row_index + 1 == at {
            "▶"
        } else {
            " "
        };
        ui.line(
            line_area,
            &Line::from_spans(vec![
                span(marker, theme.accent()),
                span(format!("{:>8} ", grouped(event.decision)), theme.muted()),
                span(
                    format!("{:<8}", truncate(&event.kind, 8)),
                    theme.tone(event.tone),
                ),
                span(
                    truncate(&event.label, usize::from(inner.width.saturating_sub(20))),
                    if past { theme.text() } else { theme.muted() },
                ),
            ]),
        );
        ui.hit(id, line_area);
    }
}

#[allow(clippy::too_many_lines)] // the list and the detail are drawn together
fn threads(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let (_, scheduler, index) = run_data(app);
    let parts = columns(area, &[Size::Fill(2), Size::Fill(3)]);
    let inner = ui.pane(
        "pane.threads",
        one(span(" Threads ", theme.strong())),
        scheduler.map(|s| one(span(format!(" {} ", s.threads.len()), theme.muted()))),
        parts[0],
        true,
    );
    let names: Vec<(String, String, String)> = scheduler.map_or_else(
        || {
            index
                .as_ref()
                .map(|i| {
                    i.threads
                        .iter()
                        .map(|n| (n.clone(), String::new(), String::new()))
                        .collect()
                })
                .unwrap_or_default()
        },
        |s| {
            s.threads
                .iter()
                .map(|t| (t.name.clone(), t.state.clone(), t.op.clone()))
                .collect()
        },
    );
    let thread_index = |name: &str| {
        index
            .as_ref()
            .and_then(|i| i.threads.iter().position(|n| n == name))
    };
    let selected = app
        .current_run()
        .and_then(|r| r.entity.as_ref())
        .and_then(|e| e.parse::<usize>().ok());
    ui.hit_as("list.threads", inner, HitRole::Scroll);
    let offset = usize::from(app.scroll.get("threads").copied().unwrap_or(0));
    for (position, (name, state, op)) in names.iter().enumerate().skip(offset) {
        let Ok(y) = u16::try_from(position - offset) else {
            break;
        };
        if y >= inner.height {
            break;
        }
        let hue = thread_index(name).unwrap_or(position);
        let id = format!("lane.{hue}");
        let line_area = row(inner, y);
        if selected == Some(hue) {
            ui.fill(line_area, theme.raised());
        } else if ui.hovered(&id) {
            ui.fill(line_area, theme.hover());
        }
        ui.line(
            line_area,
            &Line::from_spans(vec![
                span(" ●", thread_style(theme, hue)),
                span(
                    format!(" {} ", Theme::thread_glyph(state)),
                    theme.thread_state(state),
                ),
                span(format!("{:<20}", truncate(name, 20)), theme.text()),
                span(
                    format!(" {}", truncate(state, 22)),
                    theme.thread_state(state),
                ),
                span(format!(" {}", truncate(op, 14)), theme.muted()),
            ]),
        );
        ui.hit(id, line_area);
    }
    // The selected thread in detail.
    let focus = selected;
    let detail = ui.pane(
        "pane.thread",
        one(span(" Thread ", theme.strong())),
        None,
        parts[1],
        false,
    );
    let Some(focus) = focus else {
        ui.line(
            row(detail, 0),
            &one(span(" Click a thread to see it here.", theme.muted())),
        );
        return;
    };
    let name = index
        .as_ref()
        .and_then(|i| i.threads.get(focus).cloned())
        .unwrap_or_default();
    let view = scheduler.and_then(|s| s.threads.iter().find(|t| t.name == name));
    let mut lines = vec![
        Line::from_spans(vec![
            span(" ● ", thread_style(theme, focus)),
            span(name.clone(), theme.strong()),
        ]),
        Line::from_spans(vec![
            span(" state ", theme.muted()),
            span(
                view.map_or_else(|| "?".into(), |v| v.state.clone()),
                theme.thread_state(view.map_or("", |v| v.state.as_str())),
            ),
        ]),
        Line::from_spans(vec![
            span(" at    ", theme.muted()),
            span(
                view.map_or_else(|| "?".into(), |v| v.op.clone()),
                theme.text(),
            ),
        ]),
    ];
    if let Some(index) = &index
        && let Some(lane) = index.activity.get(focus)
    {
        let total: u64 = lane.iter().map(|n| u64::from(*n)).sum();
        lines.push(Line::from_spans(vec![
            span(" ran   ", theme.muted()),
            span(
                format!(
                    "{} of {} decisions",
                    grouped(total),
                    grouped(index.decisions)
                ),
                theme.text(),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(one(span(" recent decisions involving it", theme.muted())));
    if let Some(scheduler) = scheduler {
        for decision in scheduler
            .recent
            .iter()
            .rev()
            .filter(|d| d.from == name || d.to == name)
            .take(usize::from(detail.height.saturating_sub(8)))
        {
            lines.push(decision_line(theme, index.as_deref(), decision));
        }
    }
    ui.lines(detail, &lines);
    if let Some(index) = &index
        && let Some(lane) = index.activity.get(focus)
    {
        let samples: Vec<u64> = lane.iter().map(|n| u64::from(*n)).collect();
        let area = row(detail, 4);
        let spark_area = Rect::new(area.x + 40, area.y, area.width.saturating_sub(40), 1);
        let styles = SparklineStyles {
            normal: thread_style(theme, focus),
            ..SparklineStyles::from(theme.components)
        };
        sparkline(ui, "thread.activity", &samples, styles, spark_area);
    }
}

fn decision_line(
    theme: &Theme,
    index: Option<&RunIndex>,
    decision: &simvar_observer::DecisionView,
) -> Line {
    let hue = |name: &str| {
        index
            .and_then(|i| i.threads.iter().position(|n| n == name))
            .unwrap_or(0)
    };
    Line::from_spans(vec![
        span(format!(" #{:<9}", grouped(decision.number)), theme.muted()),
        span(
            truncate(&decision.from, 18),
            thread_style(theme, hue(&decision.from)),
        ),
        span(format!(" {} ", truncate(&decision.op, 12)), theme.faint()),
        span("─▶ ", theme.muted()),
        span(
            truncate(&decision.to, 18),
            thread_style(theme, hue(&decision.to)),
        ),
    ])
}

fn decisions(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let (_, scheduler, index) = run_data(app);
    let inner = ui.pane(
        "pane.decisions",
        one(span(" Decisions ", theme.strong())),
        scheduler.map(|s| {
            one(span(
                format!(" trace {:016x} ", s.trace_hash),
                theme.muted(),
            ))
        }),
        area,
        true,
    );
    let Some(scheduler) = scheduler else {
        ui.line(
            row(inner, 0),
            &one(span(
                " Pause or open a replay to see its decisions.",
                theme.muted(),
            )),
        );
        return;
    };
    ui.hit_as("list.decisions", inner, HitRole::Scroll);
    let offset = usize::from(app.scroll.get("decisions").copied().unwrap_or(0));
    let lines: Vec<Line> = scheduler
        .recent
        .iter()
        .rev()
        .skip(offset)
        .map(|d| decision_line(theme, index.as_deref(), d))
        .collect();
    ui.lines(inner, &lines);
}

fn invariants_page(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    invariants_summary(app, ui, area);
}

fn sim_view(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect, view_index: usize) {
    let theme = ui.theme;
    let Some(tab) = app.current_run() else { return };
    let Some(view) = app.views.get(view_index) else {
        return;
    };
    let run = tab.shown();
    let entities = view.entities(run);
    if let Some(entity) = &tab.entity {
        let label = entities
            .iter()
            .find(|e| &e.id == entity)
            .map_or_else(|| entity.clone(), |e| format!("{} {}", e.kind, e.label));
        let inner = ui.pane(
            "pane.entity",
            one(span(format!(" {label} "), theme.strong())),
            Some(one(span(" esc back ", theme.muted()))),
            area,
            true,
        );
        view.page(run, entity, ui, inner);
        return;
    }
    let parts = columns(area, &[Size::Fill(2), Size::Fill(3)]);
    let inner = ui.pane(
        "pane.entities",
        one(span(format!(" {} ", view.title()), theme.strong())),
        Some(one(span(format!(" {} ", entities.len()), theme.muted()))),
        parts[0],
        true,
    );
    ui.hit_as("list.entities", inner, HitRole::Scroll);
    let offset = usize::from(app.scroll.get("entities").copied().unwrap_or(0));
    for (position, entity) in entities.iter().enumerate().skip(offset) {
        let Ok(y) = u16::try_from(position - offset) else {
            break;
        };
        if y >= inner.height {
            break;
        }
        let id = format!("entity.{}", entity.id);
        let line_area = row(inner, y);
        let selected = tab.selected.as_ref() == Some(&entity.id);
        if selected {
            ui.fill(line_area, theme.raised());
        } else if ui.hovered(&id) {
            ui.fill(line_area, theme.hover());
        }
        let indent = "  ".repeat(usize::from(entity.depth));
        ui.line(
            line_area,
            &Line::from_spans(vec![
                span(if selected { "▌" } else { " " }, theme.accent()),
                span(
                    format!("{indent}{} ", truncate(&entity.kind, 10)),
                    theme.muted(),
                ),
                span(truncate(&entity.label, 24), theme.text()),
                span(
                    format!("  {}", truncate(&entity.status, 24)),
                    theme.tone(entity.tone),
                ),
            ]),
        );
        ui.hit(id, line_area);
    }
    let focus = tab.selected.clone();
    let card = ui.pane(
        "pane.card",
        one(span(" Details ", theme.strong())),
        Some(one(span(" click twice to open ", theme.muted()))),
        parts[1],
        false,
    );
    match focus {
        Some(entity) => {
            let lines = view.card(run, &entity, card.width);
            ui.lines(card, &lines);
        }
        None => ui.line(
            row(card, 0),
            &one(span(" Click something to see it here.", theme.muted())),
        ),
    }
}

fn transport(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    ui.fill(area, theme.surface());
    let playing = app
        .current_run()
        .and_then(|r| r.travel.as_ref())
        .is_some_and(|t| t.playing.is_some());
    let mut x = area.x + 1;
    for (id, label) in [
        ("transport.start", " ⏮ "),
        ("transport.previous", " ⟨◆ "),
        ("transport.back100", " ⏪ "),
        ("transport.back", " ◁ "),
        ("transport.play", if playing { " ❚❚ " } else { " ▶ " }),
        ("transport.forward", " ▷ "),
        ("transport.forward100", " ⏩ "),
        ("transport.next", " ◆⟩ "),
        ("transport.end", " ⏭ "),
    ] {
        let style = if id == "transport.play" {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        x += ui.button(id, x, area.y, label, style);
    }
    x += 1;
    // The scrubber: the whole run, click or drag to go anywhere.
    let Some((position, length)) = app.position() else {
        return;
    };
    let length = length.unwrap_or(position + 1).max(1);
    let width = area.right().saturating_sub(x + 2);
    if width < 4 {
        return;
    }
    let track = Rect::new(x, area.y, width, 1);
    let at = u16::try_from(position.min(length - 1) * u64::from(width) / length).unwrap_or(0);
    let replay = app.current_run().is_some_and(|r| r.travel.is_some());
    let done = if replay { theme.replay() } else { theme.good() };
    let line = Line::from_spans(vec![
        span("━".repeat(usize::from(at)), done),
        span("╋", done.add_modifier(Modifier::BOLD)),
        span(
            "─".repeat(usize::from(width.saturating_sub(at + 1))),
            theme.faint(),
        ),
    ]);
    ui.line(track, &line);
    ui.areas.set("scrub", track);
    ui.hit_as("scrub", track, HitRole::DragHandle);
}

fn hints(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    ui.fill(area, theme.canvas());
    let hints: Vec<KeyHint<'_>> = match app.tabs.get(app.tab) {
        Some(Tab::Run(_)) => vec![
            KeyHint::new("space", "play"),
            KeyHint::new("←→", "step"),
            KeyHint::new("⇧←→", "×100"),
            KeyHint::new("[ ]", "events"),
            KeyHint::new("b", "break"),
            KeyHint::new("< >", "pages"),
            KeyHint::new("esc", "back"),
            KeyHint::new(":", "commands"),
            KeyHint::new("?", "keys"),
        ],
        Some(Tab::Failures) => vec![
            KeyHint::new("↑↓", "select"),
            KeyHint::new("enter", "debug"),
            KeyHint::new("c", "reproduce"),
            KeyHint::new("s", "shrink"),
            KeyHint::new("x", "forget"),
            KeyHint::new("esc", "fleet"),
            KeyHint::new(":", "commands"),
            KeyHint::new("?", "keys"),
        ],
        _ => vec![
            KeyHint::new("↑↓", "select"),
            KeyHint::new("enter", "open"),
            KeyHint::new("p", "pause"),
            KeyHint::new("+ −", "workers"),
            KeyHint::new("f", "failures"),
            KeyHint::new(":", "commands"),
            KeyHint::new("t", "theme"),
            KeyHint::new("?", "keys"),
            KeyHint::new("q", "quit"),
        ],
    };
    let styles = KeyHintBarStyles {
        key: theme.accent().add_modifier(Modifier::BOLD),
        label: theme.muted(),
        ..KeyHintBarStyles::from(theme.components)
    };
    let message = app.message.as_ref().map(|(m, _)| m.clone());
    let message_width = message
        .as_ref()
        .map_or(0, |m| u16::try_from(m.chars().count() + 2).unwrap_or(0));
    let bar = KeyHintBarComponent::new("hints", &hints).styles(styles);
    ui.component(
        "hints",
        &bar,
        Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(message_width + 1),
            1,
        ),
    );
    if let Some(message) = message {
        ui.line(
            Rect::new(
                area.right().saturating_sub(message_width),
                area.y,
                message_width,
                1,
            ),
            &one(span(format!(" {message} "), theme.info())),
        );
    }
}

fn toasts(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let width = 46.min(area.width.saturating_sub(2));
    let height = 4;
    // Newest first, from just under the tabs, at most two (each fades after a few seconds,
    // or goes when clicked).
    let mut top = area.y + 2;
    for toast in app.toasts.iter().rev().take(2) {
        if top + height > area.bottom().saturating_sub(2) {
            break;
        }
        let y = top;
        let rect = Rect::new(area.right().saturating_sub(width + 1), y, width, height);
        let id = format!("toast.{}", toast.id);
        let tone = theme.tone(toast.tone);
        let glyph = match toast.tone {
            Tone::Bad => "✖",
            Tone::Warn => "▲",
            Tone::Good => "✔",
            Tone::Info | Tone::Neutral => "◆",
        };
        let inner = ui.pane(
            &format!("pane.{id}"),
            one(span(
                format!(" {glyph} {} ", toast.title),
                tone.add_modifier(Modifier::BOLD),
            )),
            Some(one(span(" ✕ ", theme.muted()))),
            rect,
            true,
        );
        ui.fill(inner, theme.raised());
        ui.lines(
            inner,
            &[
                one(span(
                    format!(
                        " {}",
                        truncate(&toast.body, usize::from(inner.width.saturating_sub(2)))
                    ),
                    theme.text(),
                )),
                one(span(
                    if toast.action.is_some() {
                        " click to open"
                    } else {
                        ""
                    },
                    theme.muted(),
                )),
            ],
        );
        ui.cx.push_hit(
            bmux_tui::hit::HitRegion::new(id, rect)
                .hoverable(true)
                .layer(5),
        );
        top = y + height;
    }
}

fn modal(ui: &mut Ui<'_, '_, '_>, area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 3,
        width,
        height,
    );
    // The scrim: everything behind is dimmed and takes no clicks.
    ui.cx
        .push_hit(bmux_tui::hit::HitRegion::new("scrim", area).layer(8));
    rect
}

fn palette(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let Some(state) = &app.palette else { return };
    let rect = modal(ui, area, 72, 18);
    let inner = ui.pane(
        "pane.palette",
        one(span(
            " Commands ",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Some(one(span(
            " goto N · back N · break N|fail|EVENT · parallel N ",
            theme.muted(),
        ))),
        rect,
        true,
    );
    ui.fill(inner, theme.raised());
    let cursor = if ui.tick % 10 < 5 { "▏" } else { " " };
    ui.line(
        row(inner, 0),
        &Line::from_spans(vec![
            span(" › ", theme.accent().add_modifier(Modifier::BOLD)),
            span(state.query.clone(), theme.strong()),
            span(cursor, theme.accent()),
        ]),
    );
    let items = app.palette_items();
    let selected = state.selected.min(items.len().saturating_sub(1));
    for (index, item) in items.iter().enumerate() {
        let Ok(y) = u16::try_from(index + 2) else {
            break;
        };
        if y >= inner.height {
            break;
        }
        let id = format!("palette.item.{index}");
        let line_area = row(inner, y);
        if index == selected || ui.hovered(&id) {
            ui.fill(line_area, theme.hover());
        }
        let hint_width = item.hint.chars().count() + 2;
        ui.line(
            line_area,
            &Line::from_spans(vec![
                span(
                    if index == selected { " ▶ " } else { "   " },
                    theme.accent(),
                ),
                span(
                    format!(
                        "{:<width$}",
                        truncate(
                            &item.label,
                            usize::from(inner.width).saturating_sub(hint_width + 4)
                        ),
                        width = usize::from(inner.width).saturating_sub(hint_width + 3)
                    ),
                    theme.text(),
                ),
                span(item.hint.clone(), theme.muted()),
            ]),
        );
        ui.cx.push_hit(
            bmux_tui::hit::HitRegion::new(id, line_area)
                .hoverable(true)
                .layer(9),
        );
    }
}

fn help(app: &App, ui: &mut Ui<'_, '_, '_>, area: Rect) {
    let theme = ui.theme;
    let rect = modal(ui, area, 78, 26);
    let inner = ui.pane(
        "pane.help",
        one(span(
            " Keys and pointer ",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Some(one(span(" any key closes ", theme.muted()))),
        rect,
        true,
    );
    ui.fill(inner, theme.raised());
    let section = |title: &str| {
        one(span(
            format!(" {title}"),
            theme.accent().add_modifier(Modifier::BOLD),
        ))
    };
    let key = |keys: &str, what: &str| {
        Line::from_spans(vec![
            span(format!("   {keys:<14}"), theme.strong()),
            span(what.to_owned(), theme.text()),
        ])
    };
    let mut lines = vec![
        section("Everywhere"),
        key(
            "click",
            "open what you click: workers, failures, events, threads, entities",
        ),
        key("click again", "open what is selected in full (entities)"),
        key(
            "wheel",
            "scroll the list under the pointer; step the timeline",
        ),
        key("tab · 1–9", "switch tabs (✕ closes a run)"),
        key("esc", "back: entity → page → run → fleet"),
        key(
            ": · ctrl+k",
            "commands (fuzzy): goto 1200, back 50, break fail, …",
        ),
        key("t · ?", "switch theme · this help"),
        section("Fleet"),
        key(
            "p · + −",
            "pause/resume the fleet · more/fewer runs at once",
        ),
        key("↑↓ enter", "pick a worker and open its live run"),
        key("click a tag", "show only runs with that tag"),
        section("Failures"),
        key(
            "enter · c · s · x",
            "debug · reproduce · shrink · forget the selected failure",
        ),
        section("A run (time travel)"),
        key("space", "play / pause (pausing a live run starts a replay)"),
        key("← → · ⇧← ⇧→", "one decision · a hundred"),
        key("[ ]", "previous / next event"),
        key("click · drag", "the timeline or scrubber: go there"),
        key("b", "break at the next decision; `break EVENT` in commands"),
        key("< >", "pages: overview, threads, invariants, decisions, …"),
    ];
    if app.views.is_empty() {
        lines.push(Line::raw(""));
    }
    ui.lines(inner, &lines);
}

/// Text of the frame, for tests.
#[cfg(test)]
pub fn frame_text(buffer: &bmux_tui::buffer::Buffer, height: u16) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for y in 0..height {
        let _ = writeln!(out, "{}", buffer.row_symbols(y).unwrap_or_default());
    }
    out
}
