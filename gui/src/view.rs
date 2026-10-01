//! The window: a connection bar, the app list on the left, the selected
//! app on the right (workers, rollout, events, logs), dialogs and toasts on
//! top. Long lists (events, logs) draw only the rows in view.

use crate::app::{
    Act, AddStatus, Conn, ConnForm, Editor, EditorStatus, FEED_ID, Gui, LOGS_ID, Message, Modal, Pending, Tab,
};
use crate::charts::{Chart, Hue, Sparkline, Unit};
use crate::format;
use crate::history::{self, Load, Range};
use crate::logs::{History, Scroll, Stream};
use crate::model::App;
use iced::widget::text::{LineHeight, Wrapping};
use iced::widget::{
    Column, Id, Row, button, canvas, center, checkbox, column, container, mouse_area, opaque, progress_bar, radio,
    responsive, row, rule, scrollable, space, stack, table, text, text_editor, text_input,
};
use iced::{Border, Center, Color, Element, Fill, Font, Length, Theme};
use warden_protocol::control::WorkerStatus;
use warden_protocol::events::AppState;

/// Height of one row in the events and logs lists.
pub const LINE_H: f32 = 17.0;
const LIST_W: f32 = 300.0;
const SMALL: f32 = 12.0;
/// The header's sparklines.
const SPARK_W: f32 = 64.0;
const SPARK_H: f32 = 18.0;

pub fn view(g: &Gui) -> Element<'_, Message> {
    let main = column![connection_bar(g), rule::horizontal(1), body(g)].height(Fill);
    let mut layers: Vec<Element<'_, Message>> = vec![main.into()];
    match &g.modal {
        Modal::None => {}
        Modal::Connection(f) => layers.push(overlay(connection_dialog(f), Some(Message::CloseModal))),
        Modal::Add(a) => layers.push(overlay(add_dialog(g, a), None)),
        Modal::Editor(e) => layers.push(overlay(editor_dialog(e), None)),
    }
    if let Some(p) = &g.confirm {
        layers.push(overlay(confirm_dialog(p), Some(Message::Cancelled)));
    }
    if !g.toasts.is_empty() {
        layers.push(toasts(g));
    }
    stack(layers).width(Fill).height(Fill).into()
}

// ------------------------------------------------------------------ styles

fn dialog_style(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    container::Style {
        background: Some(p.background.base.color.into()),
        border: Border { radius: 8.0.into(), width: 1.0, color: p.background.strong.color },
        ..container::Style::default()
    }
}

fn panel_style(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    container::Style {
        background: Some(p.background.weakest.color.into()),
        border: Border { radius: 6.0.into(), width: 1.0, color: p.background.weak.color },
        ..container::Style::default()
    }
}

fn tinted(color: fn(&Theme) -> Color) -> impl Fn(&Theme) -> container::Style {
    move |theme: &Theme| {
        let c = color(theme);
        container::Style {
            background: Some(Color { a: 0.15, ..c }.into()),
            border: Border { radius: 6.0.into(), width: 1.0, color: Color { a: 0.6, ..c } },
            ..container::Style::default()
        }
    }
}

fn warning_color(theme: &Theme) -> Color {
    theme.extended_palette().warning.base.color
}

fn danger_color(theme: &Theme) -> Color {
    theme.extended_palette().danger.base.color
}

fn success_color(theme: &Theme) -> Color {
    theme.extended_palette().success.base.color
}

/// Green that reads on the dark theme too (the palette's is dim there).
fn good(theme: &Theme) -> text::Style {
    let p = theme.extended_palette();
    let c = p.success.base.color;
    text::Style { color: Some(if p.is_dark { Color::from_rgb(0.35, 0.82, 0.55) } else { c }) }
}

fn muted(theme: &Theme) -> text::Style {
    let p = theme.extended_palette();
    text::Style { color: Some(Color { a: 0.65, ..p.background.base.text }) }
}

fn state_style(a: &App) -> fn(&Theme) -> text::Style {
    match a.entry.state {
        AppState::Running if a.is_unwell() => text::warning,
        AppState::Running => good,
        AppState::Starting => text::warning,
        AppState::Unreachable | AppState::GaveUp => text::danger,
        AppState::Stopped | AppState::NotStarted => muted,
    }
}

fn overlay<'a>(content: impl Into<Element<'a, Message>>, on_blur: Option<Message>) -> Element<'a, Message> {
    let backdrop = center(opaque(container(content).style(dialog_style).padding(20))).style(|_| container::Style {
        background: Some(Color { a: 0.55, ..Color::BLACK }.into()),
        ..Default::default()
    });
    let area = mouse_area(backdrop);
    opaque(match on_blur {
        Some(m) => area.on_press(m),
        None => area,
    })
}

fn label<'a>(t: &'a str) -> iced::widget::Text<'a> {
    text(t).size(SMALL).style(muted)
}

fn field<'a>(name: &'a str, input: impl Into<Element<'a, Message>>) -> Column<'a, Message> {
    column![label(name), input.into()].spacing(4)
}

// --------------------------------------------------------- connection bar

fn connection_bar(g: &Gui) -> Element<'_, Message> {
    let status: Element<'_, Message> = match (&g.conn, &g.model.daemon) {
        (Conn::Connected, Some(d)) => row![
            text("●").style(good),
            text(format!("wardend {} · pid {}", d.version, d.pid)),
            text(format!("· {}", g.target.describe())).size(SMALL).style(muted),
        ]
        .spacing(6)
        .align_y(Center)
        .into(),
        (Conn::Connected, None) | (Conn::Connecting { .. }, _) => {
            let what = match &g.conn {
                Conn::Connecting { what, .. } => what.clone(),
                _ => "connected; waiting for wardend".into(),
            };
            row![text("●").style(text::warning), text(format!("{what}…"))].spacing(6).align_y(Center).into()
        }
        (Conn::Down { not_running, attempt, retry_in, .. }, _) => {
            let what = if *not_running { "wardend is not running" } else { "disconnected" };
            row![
                text("●").style(text::danger),
                text(format!("{what}; reconnecting… (attempt {attempt}, every {:.1} s)", retry_in.as_secs_f32())),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        }
    };
    let host: Element<'_, Message> = match &g.model.host {
        Some(h) => {
            let series = &g.host_spark.grid.series;
            let spark = |s, hue, top: Option<f32>, floor| {
                canvas(Sparkline { series: s, hue, top, floor }).width(SPARK_W).height(SPARK_H)
            };
            row![
                text(format!("CPU {}", format::percent(h.cpu_percent))).size(13),
                spark(&series[history::HOST_CPU], Hue::Blue, None, 10.0),
                text(format!("· Mem {} / {}", format::bytes(h.mem_used_bytes), format::bytes(h.mem_total_bytes)))
                    .size(13),
                spark(&series[history::HOST_MEM], Hue::Aqua, Some(h.mem_total_bytes as f32), 0.0),
                text(format!("· Load {:.2} {:.2} {:.2}", h.load[0], h.load[1], h.load[2])).size(13),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        }
        None => space().into(),
    };
    row![
        status,
        space::horizontal(),
        host,
        button(text("Connection…").size(13)).style(button::secondary).on_press(Message::OpenConnection),
        button(text("Add app").size(13)).on_press(Message::OpenAdd),
    ]
    .spacing(14)
    .padding([8, 12])
    .align_y(Center)
    .into()
}

// -------------------------------------------------------------------- body

fn body(g: &Gui) -> Element<'_, Message> {
    if g.model.apps.is_empty() {
        return match &g.conn {
            Conn::Down { error, not_running: true, attempt, .. } => not_running(g, error, *attempt),
            Conn::Down { error, attempt, .. } => center(
                column![
                    text("Cannot reach wardend").size(20),
                    text(error.as_str()),
                    text(format!("Trying again (attempt {attempt}).")).size(SMALL).style(muted),
                ]
                .spacing(12)
                .max_width(640),
            )
            .into(),
            Conn::Connecting { what, .. } => center(text(format!("{what}…"))).into(),
            Conn::Connected => center(
                column![
                    text("No apps on this host yet").size(20),
                    text("Add one here, or with `warden start server.js --name api` in a terminal."),
                    button("Add app").on_press(Message::OpenAdd),
                ]
                .spacing(12)
                .align_x(Center),
            )
            .into(),
        };
    }
    let list = app_list(g);
    let detail: Element<'_, Message> = match g.selected_app() {
        Some(a) => detail(g, a),
        None => center(text("Select an app")).into(),
    };
    let main = row![list, rule::vertical(1), detail].height(Fill);
    match &g.conn {
        Conn::Down { error, .. } => column![
            container(text(format!("Reconnecting to wardend… {error}. What is shown is as last seen.")).size(13))
                .padding([6, 12])
                .width(Fill)
                .style(tinted(warning_color)),
            main
        ]
        .into(),
        _ => main.into(),
    }
}

fn not_running<'a>(g: &'a Gui, error: &'a str, attempt: u32) -> Element<'a, Message> {
    let where_ = if g.target.host.is_local() {
        "on this machine".to_string()
    } else {
        format!("on {} over SSH", g.target.describe())
    };
    let start = button(text(if g.starting_wardend { "Starting wardend…" } else { "Start wardend" }))
        .on_press_maybe((!g.starting_wardend).then_some(Message::StartWardend));
    center(
        column![
            text("wardend is not running").size(22),
            text(
                "wardend is the optional host daemon that streams every app's state and events to this window; \
                 your apps keep running without it."
            ),
            text(error).size(13).style(muted),
            row![start, text(format!("runs `warden daemon --background` {where_}")).size(13).style(muted)]
                .spacing(12)
                .align_y(Center),
            text(format!(
                "Or run `warden daemon --background` yourself (`warden startup` keeps it running across reboots). \
                 Trying to connect again every few seconds (attempt {attempt})."
            ))
            .size(SMALL)
            .style(muted),
        ]
        .spacing(14)
        .max_width(620),
    )
    .into()
}

// ---------------------------------------------------------------- app list

fn app_list(g: &Gui) -> Element<'_, Message> {
    let rows = g.model.sorted().into_iter().map(|a| app_row(g, a));
    scrollable(Column::with_children(rows).spacing(2).padding(6)).width(LIST_W).height(Fill).into()
}

fn app_row<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let selected = g.selected.as_deref() == Some(a.name());
    let mut parts = vec![a.entry.supervised_by.clone()];
    parts.extend(a.workers().map(|(r, c)| format!("{r}/{c} ready")));
    parts.extend(a.cpu_percent().map(format::percent));
    parts.extend(a.rss_bytes().map(format::bytes));
    let mut c = column![
        row![text(a.name()).size(15), space::horizontal(), text(a.state_label()).size(SMALL).style(state_style(a))]
            .align_y(Center),
        text(parts.join(" · ")).size(SMALL).style(muted),
    ]
    .spacing(2);
    if let Some(p) = &a.entry.problem {
        c = c.push(text(p.as_str()).size(SMALL).style(text::warning));
    }
    button(c)
        .width(Fill)
        .padding([6, 8])
        .style(if selected { button::subtle } else { button::text })
        .on_press(Message::Select(a.name().to_string()))
        .into()
}

// ------------------------------------------------------------------ detail

fn detail<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let e = &a.entry;
    let mut head = row![text(a.name()).size(22), text(a.state_label()).style(state_style(a)), space::horizontal()]
        .spacing(12)
        .align_y(Center);
    if let Some(cfg) = &e.config {
        head = head.push(text(cfg.as_str()).size(SMALL).style(muted)).push(
            button(text("Edit config").size(13)).style(button::secondary).on_press(Message::OpenEditor(e.name.clone())),
        );
    }
    let mut facts = vec![format!("supervised by {}", e.supervised_by)];
    if let Some(p) = e.supervisor_pid {
        facts.push(format!("supervisor pid {p}"));
    }
    if e.supervisor_restarts > 0 {
        facts.push(format!("{} supervisor restarts", e.supervisor_restarts));
    }
    if let Some(s) = &a.status {
        facts.push(format!("{} mode", s.mode));
        facts.push(format!("up {}", format::duration(s.uptime_secs)));
        if let Some(r) = s.supervisor_rss_bytes {
            facts.push(format!("supervisor {}", format::bytes(r)));
        }
        if s.health_suspended {
            facts.push("health replacements suspended".into());
        }
    }
    let mut c = column![head, text(facts.join(" · ")).size(13).style(muted)].spacing(10);
    if let Some(p) = &e.problem {
        c = c.push(container(text(p.as_str()).size(13)).padding([8, 10]).width(Fill).style(tinted(warning_color)));
    }
    c = c.push(actions(g, a));
    if let Some(r) = rollout(a) {
        c = c.push(r);
    }
    c = c.push(workers(g, a));
    let tab = |label: &'static str, t: Tab| {
        button(text(label).size(13))
            .style(if g.tab == t { button::primary } else { button::secondary })
            .on_press(Message::Tab(t))
    };
    c = c.push(row![tab("Events", Tab::Events), tab("Logs", Tab::Logs), tab("History", Tab::History)].spacing(6));
    c = c.push(match g.tab {
        Tab::Events => events(a, g.feed_scroll),
        Tab::Logs => logs_pane(g),
        Tab::History => history_pane(g),
    });
    c.padding(12).width(Fill).height(Fill).into()
}

fn actions<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let up = g.connected() && a.supervisor_up();
    let stopped = a.status.as_ref().is_some_and(|s| s.stopped);
    let busy = |act: Act| g.busy.iter().any(|(n, x)| n == a.name() && *x == act);
    let named = |act: Act, name: String, enabled: bool, style: fn(&Theme, button::Status) -> button::Style| {
        let label = if busy(act) { format!("{name}…") } else { name };
        button(text(label).size(13))
            .style(style)
            .padding([5, 10])
            .on_press_maybe((enabled && !busy(act)).then(|| Message::Act(a.name().to_string(), act)))
    };
    let btn = |act: Act, enabled: bool, style: fn(&Theme, button::Status) -> button::Style| {
        named(act, act.label(), enabled, style)
    };
    let count = a.status.as_ref().map(|s| s.workers_configured).unwrap_or(0);
    let scale = row![
        named(Act::Scale(count.saturating_sub(1)), "−".into(), up && count > 0, button::secondary),
        text(format!("{count} workers")).size(13),
        named(Act::Scale(count + 1), "+".into(), up, button::secondary),
    ]
    .spacing(6)
    .align_y(Center);
    let start_ok = g.connected() && (!a.supervisor_up() || stopped);
    row![
        btn(Act::Reload, up && !stopped, button::primary),
        btn(Act::SafeReload, up && !stopped, button::secondary),
        btn(Act::RollingRestart, up && !stopped, button::secondary),
        btn(Act::HardRestart, up && !stopped, button::danger),
        btn(Act::Reset, up, button::secondary),
        btn(Act::Stop, up && !stopped, button::danger),
        btn(Act::Start, start_ok, button::success),
        space().width(16),
        scale,
    ]
    .spacing(6)
    .align_y(Center)
    .wrap()
    .into()
}

fn rollout(a: &App) -> Option<Element<'_, Message>> {
    let mut c = column![].spacing(6);
    if let Some(r) = &a.rollout {
        let total = r.total.max(1) as f32;
        c = c.push(
            text(format!("{} {}/{}: {} ({})", r.kind, r.done, r.total, r.phase, format::duration(r.elapsed_secs)))
                .size(13),
        );
        c = c.push(progress_bar(0.0..=total, r.done as f32).girth(8));
    }
    if let Some(o) = &a.last_outcome {
        let t = text(format!(
            "Last {}: {} ({:.1} s): {}",
            o.kind,
            if o.ok { "ok" } else { "FAILED" },
            o.duration_secs,
            o.message
        ))
        .size(13);
        c = c.push(if o.ok { t.style(muted) } else { t.style(text::danger) });
    }
    (a.rollout.is_some() || a.last_outcome.is_some()).then(|| c.into())
}

fn workers<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let Some(s) = &a.status else {
        return text("No workers to show: the supervisor is not running (or wardend does not watch it yet).")
            .size(13)
            .style(muted)
            .into();
    };
    if s.workers.is_empty() {
        return text("No workers (scaled to 0).").size(13).style(muted).into();
    }
    let h = |t: &'static str| text(t).size(SMALL).style(muted);
    let cell = |t: String| text(t).size(13);
    let can_restart = g.connected() && a.supervisor_up() && !s.stopped;
    let app = a.name().to_string();
    let columns = vec![
        table::column(h("Worker"), |w: &WorkerStatus| cell(w.id.to_string())),
        table::column(h("State"), |w: &WorkerStatus| {
            let t = cell(w.state.clone());
            match w.state.as_str() {
                "RUNNING" => t.style(good),
                "FAILED" | "CRASHED" => t.style(text::danger),
                _ => t.style(text::warning),
            }
        }),
        table::column(h("PID"), |w: &WorkerStatus| cell(format::opt(w.pid, |p| p.to_string()))),
        table::column(h("Uptime"), |w: &WorkerStatus| cell(format::opt(w.uptime_secs, format::duration))),
        table::column(h("Restarts"), |w: &WorkerStatus| cell(w.restarts.to_string())),
        table::column(h("CPU"), |w: &WorkerStatus| cell(format::opt(w.cpu_percent, format::percent))),
        table::column(h("RSS"), |w: &WorkerStatus| cell(format::opt(w.rss_bytes, format::bytes))),
        table::column(h("Health"), |w: &WorkerStatus| {
            let t = cell(format::health(w.healthy).into());
            match w.healthy {
                Some(false) => t.style(text::danger),
                _ => t,
            }
        }),
        table::column(h("Last exit"), |w: &WorkerStatus| cell(w.last_exit.clone().unwrap_or_else(|| "-".into()))),
        table::column(h(""), move |w: &WorkerStatus| {
            button(text("restart").size(SMALL))
                .padding([2, 8])
                .style(button::secondary)
                .on_press_maybe(can_restart.then(|| Message::Act(app.clone(), Act::RestartWorker(w.id))))
        }),
    ];
    let t = table(columns, s.workers.iter()).padding_x(10).padding_y(4);
    container(scrollable(t).width(Fill)).max_height(240).into()
}

// ---------------------------------------------------------- events & logs

/// A list drawn only where it is visible: every row is `LINE_H` tall, the
/// rest is spacers. Anchored at the bottom, like a terminal.
fn lines<'a>(
    items: Vec<(&'a str, Stream)>,
    scroll: Scroll,
    id: &'static str,
    on_scroll: fn(f32) -> Message,
) -> Element<'a, Message> {
    let total = items.len();
    let list = responsive(move |size| {
        let (start, end) = scroll.window(total, LINE_H, size.height);
        let mut col = Column::new().width(Fill);
        if start > 0 {
            col = col.push(space().height(start as f32 * LINE_H));
        }
        for (t, kind) in &items[start..end] {
            let line = text(*t)
                .font(Font::MONOSPACE)
                .size(12)
                .line_height(LineHeight::Absolute(LINE_H.into()))
                .wrapping(Wrapping::None);
            col = col.push(match kind {
                Stream::Stderr => line.style(text::danger),
                Stream::Note => line.style(text::warning),
                Stream::Event => line.style(muted),
                Stream::Stdout => line,
            });
        }
        if end < total {
            col = col.push(space().height((total - end) as f32 * LINE_H));
        }
        scrollable(col.padding([4, 8]))
            .id(Id::new(id))
            .anchor_bottom()
            .width(Fill)
            .height(Fill)
            .on_scroll(move |v| on_scroll(v.absolute_offset().y))
            .into()
    });
    container(list).style(panel_style).width(Fill).height(Fill).into()
}

fn events(a: &App, scroll: Scroll) -> Element<'_, Message> {
    if a.feed.is_empty() {
        return container(text("No events yet.").size(13).style(muted)).padding(8).into();
    }
    let items = a.feed.iter().map(|l| (l.as_str(), Stream::Stdout)).collect();
    lines(items, scroll, FEED_ID, Message::FeedScrolled)
}

fn logs_pane(g: &Gui) -> Element<'_, Message> {
    let Some(p) = &g.logs else { return space().into() };
    let pause = if p.paused {
        button(text(format!("Resume ({} new)", p.held())).size(13))
            .style(button::primary)
            .on_press(Message::LogsPause(false))
    } else {
        button(text("Pause").size(13)).style(button::secondary).on_press(Message::LogsPause(true))
    };
    let mut notes = Vec::new();
    match &p.history {
        History::Loading => notes.push("loading recent lines…".to_string()),
        History::Failed(e) => notes.push(format!("recent lines unavailable: {e}")),
        History::Loaded => {}
    }
    if !g.logs_live {
        notes.push("connecting…".into());
    }
    if p.skipped > 0 {
        notes.push(format!("{} lines skipped", p.skipped));
    }
    let bar = row![
        checkbox(p.stdout).label("stdout").on_toggle(Message::LogsStdout).size(14).text_size(13),
        checkbox(p.stderr).label("stderr").on_toggle(Message::LogsStderr).size(14).text_size(13),
        checkbox(p.events).label("events").on_toggle(Message::LogsEvents).size(14).text_size(13),
        text_input("filter", &p.filter).on_input(Message::LogsFilter).size(13).width(220).padding([4, 8]),
        pause,
        button(text("Clear").size(13)).style(button::secondary).on_press(Message::LogsClear),
        text(notes.join(" · ")).size(SMALL).style(muted),
    ]
    .spacing(10)
    .align_y(Center);
    let visible = p.visible();
    let body = if visible.is_empty() {
        container(text(if p.is_empty() { "No log lines yet." } else { "No line matches." }).size(13).style(muted))
            .padding(8)
            .height(Fill)
            .into()
    } else {
        let items = visible.iter().map(|l| (l.text.as_str(), l.stream)).collect();
        lines(items, p.scroll, LOGS_ID, Message::LogsScrolled)
    };
    column![bar, body].spacing(8).height(Fill).into()
}

// ----------------------------------------------------------------- history

/// How a chart draws its series.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    /// A line over a wash (CPU, memory).
    Area,
    /// A line only (a level that mostly sits at the top: workers ready).
    Line,
    /// A bar per point (restarts).
    Bars,
}

/// The History tab: the range buttons, then CPU and memory, restarts and
/// workers ready, each a chart card.
fn history_pane(g: &Gui) -> Element<'_, Message> {
    let Some(c) = &g.chart else { return space().into() };
    let ranges = Row::with_children(Range::ALL.map(|r| {
        button(text(r.label()).size(13))
            .style(if c.range == r { button::primary } else { button::secondary })
            .padding([4, 14])
            .on_press(Message::HistoryRange(r))
            .into()
    }))
    .spacing(4);
    let per_point = match c.grid.step_s {
        s if s < 60 => format!("{s} s per point"),
        s => format!("{} min per point", s / 60),
    };
    let note: Element<'_, Message> = match &c.load {
        Load::Loading => text("loading…").size(SMALL).style(muted).into(),
        Load::Failed(e) => text(e.as_str()).size(SMALL).style(text::danger).into(),
        Load::Ready => text(format!("{per_point} · from wardend, then live")).size(SMALL).style(muted).into(),
    };
    let bar = row![ranges, note].spacing(14).align_y(Center);
    if c.grid.is_empty() && matches!(c.load, Load::Failed(_)) {
        return column![bar].into();
    }
    let span = c.range.secs();
    let s = &c.grid.series;
    let range = c.range.label();
    let cpu = match s[history::CPU].stats() {
        Some((avg, peak)) => format!("avg {} · peak {}", format::percent(avg.into()), format::percent(peak.into())),
        None => "no samples".into(),
    };
    let mem = match (s[history::MEM].last(), s[history::MEM].stats()) {
        (Some(now), Some((_, peak))) => {
            format!("now {} · peak {}", format::bytes(now as u64), format::bytes(peak as u64))
        }
        _ => "no samples".into(),
    };
    let restarts = match s[history::RESTARTS].sum() as u64 {
        _ if s[history::RESTARTS].stats().is_none() => "no samples".into(),
        0 => format!("none in {range}"),
        n => format!("{n} in {range}"),
    };
    let ready = match (s[history::READY].stats(), s[history::CONFIGURED].stats()) {
        (Some(_), Some((_, configured))) => {
            let fewest = s[history::READY].values.iter().flatten().fold(f32::MAX, |m, v| m.min(*v));
            if fewest >= configured {
                format!("all {configured:.0} ready throughout")
            } else {
                format!("fewest {fewest:.0} of {configured:.0}")
            }
        }
        _ => "no samples".into(),
    };
    let card = |title: &'static str, sub: &'static str, summary: String, i: usize, unit: Unit, hue: Hue, look: Look| {
        let chart = Chart {
            series: &s[i],
            start_s: c.grid.start_s,
            step_s: c.grid.step_s,
            end_s: if c.grid.is_empty() { warden_protocol::events::now_ms() / 1000 } else { c.grid.end_s() },
            span_s: span,
            unit,
            hue,
            bars: look == Look::Bars,
            area: look == Look::Area,
            faded: c.load == Load::Loading,
        };
        container(
            column![
                row![
                    text(title).size(14),
                    text(sub).size(SMALL).style(muted),
                    space::horizontal(),
                    text(summary).size(SMALL).style(muted),
                ]
                .spacing(8)
                .align_y(Center),
                canvas(chart).width(Fill).height(Fill),
            ]
            .spacing(4),
        )
        .padding([8, 10])
        .width(Fill)
        .height(Fill)
        .style(panel_style)
    };
    column![
        bar,
        row![
            card("CPU", "% of one core", cpu, history::CPU, Unit::Percent, Hue::Blue, Look::Area),
            card("Memory", "resident, with the supervisor", mem, history::MEM, Unit::Bytes, Hue::Aqua, Look::Area),
        ]
        .spacing(10)
        .height(Length::FillPortion(1)),
        row![
            card("Restarts", "per point", restarts, history::RESTARTS, Unit::Count, Hue::Orange, Look::Bars),
            card("Workers ready", "the fewest per point", ready, history::READY, Unit::Count, Hue::Violet, Look::Line),
        ]
        .spacing(10)
        .height(Length::FillPortion(1)),
    ]
    .spacing(10)
    .height(Fill)
    .into()
}

// ----------------------------------------------------------------- dialogs

fn confirm_dialog(p: &Pending) -> Element<'_, Message> {
    column![
        text(p.act.question(&p.app)).size(15),
        row![
            space::horizontal(),
            button("Cancel").style(button::secondary).on_press(Message::Cancelled),
            button(text(p.act.label())).style(button::danger).on_press(Message::Confirmed),
        ]
        .spacing(10),
    ]
    .spacing(18)
    .width(480)
    .into()
}

fn connection_dialog(f: &ConnForm) -> Element<'_, Message> {
    let mut c = column![
        text("Connect to").size(18),
        row![
            radio("This machine", false, Some(f.ssh), Message::ConnSsh),
            radio("A remote host over SSH", true, Some(f.ssh), Message::ConnSsh),
        ]
        .spacing(20),
    ]
    .spacing(14)
    .width(540);
    if f.ssh {
        c = c
            .push(field(
                "SSH target",
                text_input("user@host (or a Host from ~/.ssh/config)", &f.dest)
                    .on_input(Message::ConnDest)
                    .on_submit(Message::ApplyConnection),
            ))
            .push(field(
                "wardend's socket there",
                text_input("/run/warden/wardend.sock", &f.remote_socket).on_input(Message::ConnRemoteSocket),
            ))
            .push(field(
                "The warden CLI there (for Add app, Edit config, Start wardend)",
                text_input("warden", &f.remote_warden).on_input(Message::ConnRemoteWarden),
            ))
            .push(
                text(
                    "The GUI runs `ssh -N -L <local socket>:<remote socket> <target>` and speaks to wardend through \
                     it. It never asks for a password: ssh uses your agent and keys (BatchMode). wardend run by \
                     root listens on /run/warden/wardend.sock (macOS: /var/run/warden/wardend.sock); run by a user, \
                     on /run/user/<uid>/warden/wardend.sock.",
                )
                .size(SMALL)
                .style(muted),
            );
    } else {
        let default = crate::client::local_socket().display().to_string();
        c = c.push(field(
            "wardend's socket (empty: the default)",
            text_input(&default, &f.socket).on_input(Message::ConnSocket).on_submit(Message::ApplyConnection),
        ));
    }
    if let Some(e) = &f.error {
        c = c.push(text(e.as_str()).size(13).style(text::danger));
    }
    c.push(
        row![
            space::horizontal(),
            button("Cancel").style(button::secondary).on_press(Message::CloseModal),
            button("Connect").on_press(Message::ApplyConnection),
        ]
        .spacing(10),
    )
    .into()
}

fn add_dialog<'a>(g: &'a Gui, a: &'a crate::app::AddForm) -> Element<'a, Message> {
    let f = &a.form;
    let running = a.status == AddStatus::Running;
    let input = |ph: &'a str, v: &'a str, m: fn(String) -> Message| {
        let t = text_input(ph, v).padding([5, 8]);
        if running { t } else { t.on_input(m).on_submit(Message::SubmitAdd) }
    };
    let preview = match f.args() {
        Ok(args) => {
            let mut argv = vec!["warden".to_string()];
            argv.extend(args);
            format!("$ {}", crate::ssh::shell_join(&argv))
        }
        Err(_) => "$ warden start <script or command> --name <name>".into(),
    };
    let where_ = if g.target.host.is_local() { "this machine".to_string() } else { g.target.describe() };
    let mut c = column![
        text("Add an app").size(18),
        text(format!("Runs `warden start` on {where_}; it waits until the app is up and says why if it is not."))
            .size(SMALL)
            .style(muted),
        field(
            "Script, program or command line",
            input("server.js, or: python3 -m http.server 8000", &f.what, Message::AddWhat)
        ),
        row![
            field("Name", input("api", &f.name, Message::AddName)).width(Fill),
            field("Instances", input("1, 4 or max", &f.instances, Message::AddInstances)).width(140),
            field("Port", input("3000 (optional)", &f.port, Message::AddPort)).width(140),
        ]
        .spacing(10),
        field(
            "Env file (optional; KEY=VALUE lines, on this machine)",
            input("/srv/api/.env", &f.env_file, Message::AddEnv),
        ),
        text(preview).font(Font::MONOSPACE).size(SMALL),
    ]
    .spacing(12)
    .width(620);
    match &a.status {
        AddStatus::Editing => {}
        AddStatus::Running => c = c.push(text("Running… (`warden start` waits until the app is up)").size(13)),
        AddStatus::Done(Err(e)) => c = c.push(text(e.as_str()).size(13).style(text::danger)),
        AddStatus::Done(Ok(added)) => {
            let out = &added.output;
            let mut log = out.text();
            for n in &added.notes {
                log.push('\n');
                log.push_str(n);
            }
            c = c.push(
                text(if out.ok { "Done:" } else { "warden start failed:" }.to_string()).size(13).style(if out.ok {
                    good
                } else {
                    text::danger
                }),
            );
            c = c.push(
                container(scrollable(text(log).font(Font::MONOSPACE).size(SMALL)).height(Length::Shrink))
                    .max_height(220)
                    .padding(8)
                    .width(Fill)
                    .style(panel_style),
            );
        }
    }
    c.push(
        row![
            space::horizontal(),
            button("Close").style(button::secondary).on_press_maybe((!running).then_some(Message::CloseModal)),
            button(if running { "Adding…" } else { "Add" }).on_press_maybe((!running).then_some(Message::SubmitAdd)),
        ]
        .spacing(10),
    )
    .into()
}

fn editor_dialog(e: &Editor) -> Element<'_, Message> {
    let ready = e.status == EditorStatus::Ready;
    let status = match e.status {
        EditorStatus::Loading => "loading…",
        EditorStatus::Checking => "checking…",
        EditorStatus::Saving => "saving…",
        EditorStatus::Ready if e.dirty => "modified",
        EditorStatus::Ready => "",
    };
    let mut c = column![
        row![
            text(format!("{}: {}", e.app, e.path)).size(15),
            space::horizontal(),
            text(status).size(SMALL).style(muted),
        ]
        .align_y(Center),
        text_editor(&e.content).on_action(Message::Edit).font(Font::MONOSPACE).size(13).height(Fill),
    ]
    .spacing(10)
    .width(860)
    .height(620);
    match &e.result {
        Some(Ok(m)) => c = c.push(text(format!("✓ {m}")).size(13).style(good)),
        Some(Err(m)) => {
            c = c.push(container(scrollable(text(m.as_str()).size(13).style(text::danger))).max_height(120).width(Fill))
        }
        None => {}
    }
    let mut buttons = row![
        text("Validate runs `warden check -c` on the text; Save validates, then writes the file.")
            .size(SMALL)
            .style(muted),
        space::horizontal(),
        button("Close")
            .style(button::secondary)
            .on_press_maybe((e.status != EditorStatus::Saving).then_some(Message::CloseModal)),
        button("Validate").style(button::secondary).on_press_maybe(ready.then_some(Message::Validate)),
        button("Save").on_press_maybe((ready && e.dirty).then_some(Message::Save)),
    ]
    .spacing(10)
    .align_y(Center);
    if e.saved {
        buttons = buttons.push(button("Reload now").style(button::success).on_press(Message::ReloadAfterSave));
    }
    c.push(buttons).into()
}

// ------------------------------------------------------------------ toasts

fn toasts(g: &Gui) -> Element<'_, Message> {
    let list = Column::with_children(g.toasts.iter().map(|t| {
        let color: fn(&Theme) -> Color = if t.ok { success_color } else { danger_color };
        container(
            row![
                text(t.text.as_str()).size(13).width(Fill),
                button(text("×").size(14)).style(button::text).padding([0, 6]).on_press(Message::DismissToast(t.id)),
            ]
            .spacing(8)
            .align_y(Center),
        )
        .padding([8, 12])
        .width(440)
        .style(move |theme: &Theme| {
            let mut s = tinted(color)(theme);
            // Opaque enough to read over the lists.
            s.background = Some(theme.extended_palette().background.strong.color.into());
            s
        })
        .into()
    }))
    .spacing(8);
    container(list).width(Fill).height(Fill).align_right(Fill).align_bottom(Fill).padding(16).into()
}
