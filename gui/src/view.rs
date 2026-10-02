//! The window: a top bar (the connection, the host's load, Add app), the
//! app list on the left, the selected app on the right (what it is doing,
//! its actions, its workers, then events, logs and history), dialogs and
//! toasts on top. Long lists (events, logs) draw only the rows in view.

use crate::app::{
    Act, AddStatus, Conn, Editor, EditorStatus, FEED_ID, Gui, LOGS_ID, MachineForm, MenuKind, Message, Modal, Pending,
    Tab,
};
use crate::charts::{Chart, Hue, Sparkline, Unit};
use crate::dropdown::Dropdown;
use crate::format::{self, Severity};
use crate::history::{self, Load, Range};
use crate::icons::Icon;
use crate::logs::{History, Scroll, Stream};
use crate::look::{
    self, DISPLAY, DISPLAY_BOLD, MEDIUM, MONO, SEMIBOLD, Tone, badge, dot, heading, icon, labeled, muted, section,
    state_pill, tile, tip,
};
use crate::model::App;
use iced::widget::text::{LineHeight, Wrapping};
use iced::widget::{
    Column, Id, Row, button, canvas, center, checkbox, column, container, mouse_area, opaque, progress_bar, responsive,
    row, rule, scrollable, space, stack, table, text, text_editor, text_input,
};
use iced::{Center, Color, Element, Fill, Length, Theme};
use std::borrow::Cow;
use warden_protocol::control::WorkerStatus;
use warden_protocol::events::AppState;

/// Height of one row in the events and logs lists.
pub const LINE_H: f32 = 19.0;
const LIST_W: f32 = 320.0;

/// A slim round scrollbar, so lists do not look like a file manager.
fn thin() -> scrollable::Scrollbar {
    scrollable::Scrollbar::new().width(6).scroller_width(6).margin(3)
}
const SMALL: f32 = 12.0;
// The heights of the detail page's parts, to tell whether the bottom pane still fits.
const TOPBAR_H: f32 = 55.0;
const PAGE_PAD: f32 = 18.0;
const GAP: f32 = 14.0;
const HEAD_H: f32 = 44.0;
const FACTS_H: f32 = 18.0;
const BANNER_H: f32 = 40.0;
const IDLE_H: f32 = 92.0;
const PORTS_H: f32 = 24.0;
const ACTIONS_H: f32 = 32.0;
const ROLLOUT_H: f32 = 71.0;
const TABS_H: f32 = 37.0;
/// The bottom pane (events, logs, history) keeps at least this much, else the page scrolls.
const PANE_MIN_H: f32 = 120.0;
/// The top bar's sparklines.
const SPARK_W: f32 = 56.0;
const SPARK_H: f32 = 18.0;

pub fn view(g: &Gui) -> Element<'_, Message> {
    let main = column![topbar(g), rule::horizontal(1), body(g)].height(Fill);
    let mut layers: Vec<Element<'_, Message>> = vec![main.into()];
    match &g.modal {
        Modal::None => {}
        Modal::Machine(f) => layers.push(overlay(machine_dialog(f), Some(Message::CloseModal))),
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

// ------------------------------------------------------------------ shared

fn overlay<'a>(content: impl Into<Element<'a, Message>>, on_blur: Option<Message>) -> Element<'a, Message> {
    let backdrop = center(opaque(container(content).style(look::dialog).padding(22))).style(|_| container::Style {
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
    column![label(name), input.into()].spacing(5)
}

fn small<'a>(t: impl Into<String>) -> iced::widget::Text<'a> {
    text(t.into()).size(SMALL).style(muted)
}

fn state_tone(a: &App) -> Tone {
    match a.entry.state {
        AppState::Running if a.is_unwell() => Tone::Warn,
        AppState::Running if a.state_label() == "rolling" => Tone::Accent,
        AppState::Running => Tone::Good,
        AppState::Starting => Tone::Warn,
        AppState::Unreachable | AppState::GaveUp => Tone::Bad,
        AppState::Stopped | AppState::NotStarted => Tone::Muted,
    }
}

fn severity_tone(s: Severity) -> Tone {
    match s {
        Severity::Bad => Tone::Bad,
        Severity::Warn => Tone::Warn,
        Severity::Good => Tone::Good,
        Severity::Info => Tone::Muted,
    }
}

/// `text`, at most `max` characters, with `…` where it was cut.
fn clip(text: &str, max: usize) -> Cow<'_, str> {
    if text.chars().count() <= max {
        return Cow::Borrowed(text);
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    Cow::Owned(format!("{}…", cut.trim_end()))
}

fn menu_item<'a>(
    icon_: Option<Icon>,
    tone: Tone,
    title: String,
    hint: Option<String>,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let lead: Element<'a, Message> = match icon_ {
        Some(i) => container(icon(i).size(15).style(tone.style())).width(22).into(),
        None => space().width(22).into(),
    };
    let mut c = column![text(title).size(13).font(MEDIUM).style(if tone == Tone::Bad {
        tone.style()
    } else {
        Tone::Plain.style()
    })]
    .spacing(1);
    if let Some(h) = hint {
        c = c.push(small(h));
    }
    button(row![lead, c].spacing(8).align_y(Center))
        .width(Fill)
        .padding([6, 10])
        .style(look::ghost)
        .on_press_maybe(on_press)
        .into()
}

// ---------------------------------------------------------------- top bar

fn topbar(g: &Gui) -> Element<'_, Message> {
    let brand = row![look::brand_mark(30.0), heading("Warden").size(18)].spacing(10).align_y(Center);
    let daemon: Element<'_, Message> = match (&g.conn, &g.model.daemon) {
        (Conn::Connected, Some(d)) => {
            text(format!("wardend {} · pid {}", d.version, d.pid)).size(SMALL).style(muted).into()
        }
        (Conn::Connected, None) | (Conn::Connecting { .. }, _) => {
            let what = match &g.conn {
                Conn::Connecting { what, .. } => what.clone(),
                _ => "connected; waiting for wardend".into(),
            };
            small(format!("{what}…")).into()
        }
        (Conn::Down { not_running, attempt, retry_in, .. }, _) => {
            let what = if *not_running { "wardend is not running" } else { "disconnected" };
            let when = match retry_in.as_secs() {
                s if s >= 60 => format!("next try in {} min", s / 60),
                _ => format!("reconnecting… (attempt {attempt}, every {:.1} s)", retry_in.as_secs_f32()),
            };
            text(format!("{what}; {when}")).size(SMALL).style(Tone::Bad.style()).into()
        }
    };
    // As the window narrows the top bar drops what it can spare: the sparklines, then the
    // load and the memory, then wardend's version (the machine and Add app always stay).
    let width = g.window.width;
    let (sparks, mem, load, version) = (width >= 1240.0, width >= 1000.0, width >= 1080.0, width >= 960.0);
    let host: Element<'_, Message> = match &g.model.host {
        Some(h) => {
            let series = &g.host_spark.grid.series;
            let spark = |s, hue, top: Option<f32>, floor| {
                canvas(Sparkline { series: s, hue, top, floor }).width(SPARK_W).height(SPARK_H)
            };
            let metric =
                |i: Icon, t: String| row![icon(i).size(13).style(muted), text(t).size(13)].spacing(5).align_y(Center);
            let mut r = Row::new().spacing(8).align_y(Center);
            r = r.push(metric(Icon::Cpu, format!("CPU {}", format::percent(h.cpu_percent))));
            if sparks {
                r = r.push(spark(&series[history::HOST_CPU], Hue::Blue, None, 10.0));
            }
            if mem {
                r = r.push(metric(
                    Icon::Memory,
                    format!("Mem {} / {}", format::bytes(h.mem_used_bytes), format::bytes(h.mem_total_bytes)),
                ));
                if sparks {
                    r = r.push(spark(&series[history::HOST_MEM], Hue::Aqua, Some(h.mem_total_bytes as f32), 0.0));
                }
            }
            if load {
                r = r.push(metric(Icon::Activity, format!("Load {:.2} {:.2} {:.2}", h.load[0], h.load[1], h.load[2])));
            }
            r.into()
        }
        None => space().into(),
    };
    let daemon: Element<'_, Message> =
        if version || !matches!(g.conn, Conn::Connected) { daemon } else { space().into() };
    row![
        brand,
        space().width(6),
        daemon,
        space::horizontal(),
        host,
        machines_button(g),
        button(labeled(Icon::Plus, "Add app"))
            .padding([7, 16])
            .style(look::solid(Tone::Plain))
            .on_press(Message::OpenAdd),
    ]
    .spacing(14)
    .padding([11, 18])
    .align_y(Center)
    .into()
}

/// `● Connected · This machine ▾`: the machine the window shows, always one
/// (this one at first), and the menu to pick another or add an SSH machine.
fn machines_button(g: &Gui) -> Element<'_, Message> {
    let (tone, word) = match (&g.conn, &g.model.daemon) {
        (Conn::Connected, Some(_)) => (Tone::Good, "Connected"),
        (Conn::Down { .. }, _) => (Tone::Bad, "Disconnected"),
        _ => (Tone::Warn, "Connecting"),
    };
    let open = g.menu == Some(MenuKind::Machines);
    let main = button(
        row![
            dot(tone, 9.0),
            text(word).size(13).font(MEDIUM),
            text(g.target.machine_name()).size(13).style(muted),
            icon(if open { Icon::ChevronUp } else { Icon::ChevronDown }).size(14).style(muted),
        ]
        .spacing(8)
        .align_y(Center),
    )
    .padding([7, 14])
    .style(look::quiet)
    .on_press(Message::ToggleMenu(MenuKind::Machines));
    Dropdown::new(main, machines_menu(g), open).on_dismiss(Message::CloseMenu).into()
}

fn machines_menu(g: &Gui) -> Element<'_, Message> {
    let current_ssh = match &g.target.endpoint {
        crate::client::Endpoint::Ssh(t) => Some(t.dest.as_str()),
        crate::client::Endpoint::Socket(_) => None,
    };
    let tick = |on: bool| -> Element<'_, Message> {
        container(if on {
            icon(Icon::Check).size(14).style(Tone::Accent.style())
        } else {
            icon(Icon::Check).size(14).style(|_: &Theme| iced::widget::text::Style { color: Some(Color::TRANSPARENT) })
        })
        .width(20)
        .into()
    };
    let mut c = column![section("Machine")].spacing(2).padding([4, 6]);
    c = c.push(
        button(
            row![
                tick(current_ssh.is_none()),
                icon(Icon::Laptop).size(15).style(muted),
                column![text("This machine").size(13).font(MEDIUM), small("the default, always available")].spacing(1),
            ]
            .spacing(8)
            .align_y(Center),
        )
        .width(Fill)
        .padding([6, 8])
        .style(look::ghost)
        .on_press(Message::UseThisMachine),
    );
    let mut listed_current = false;
    for m in &g.saved.machines {
        let is_current = current_ssh == Some(m.dest.as_str());
        listed_current |= is_current;
        c = c.push(
            row![
                button(
                    row![
                        tick(is_current),
                        icon(Icon::Globe).size(15).style(muted),
                        column![text(m.dest.clone()).size(13).font(MEDIUM), small(m.remote_socket.clone())].spacing(1),
                    ]
                    .spacing(8)
                    .align_y(Center),
                )
                .width(Fill)
                .padding([6, 8])
                .style(look::ghost)
                .on_press(Message::UseMachine(m.dest.clone())),
                tip(
                    button(icon(Icon::Edit).size(14))
                        .padding(6)
                        .style(look::ghost)
                        .on_press(Message::EditMachine(m.dest.clone())),
                    "Edit"
                ),
                tip(
                    button(icon(Icon::Close).size(14))
                        .padding(6)
                        .style(look::ghost)
                        .on_press(Message::ForgetMachine(m.dest.clone())),
                    "Remove from the list"
                ),
            ]
            .align_y(Center),
        );
    }
    // Opened with `--ssh`: connected, but not in the list.
    if let (Some(dest), false) = (current_ssh, listed_current) {
        c = c.push(
            row![
                tick(true),
                icon(Icon::Globe).size(15).style(muted),
                column![text(dest.to_string()).size(13).font(MEDIUM), small("from --ssh, not saved")].spacing(1),
            ]
            .spacing(8)
            .align_y(Center)
            .padding([6, 8]),
        );
    }
    c = c.push(rule::horizontal(1));
    c = c.push(
        button(
            row![container(icon(Icon::Plus).size(15)).width(20), text("Add SSH machine").size(13).font(MEDIUM)]
                .spacing(8)
                .align_y(Center),
        )
        .width(Fill)
        .padding([6, 8])
        .style(look::ghost)
        .on_press(Message::AddMachine),
    );
    container(c).width(340).padding(4).style(look::menu).into()
}

// -------------------------------------------------------------------- body

fn body(g: &Gui) -> Element<'_, Message> {
    if g.model.apps.is_empty() {
        return match &g.conn {
            Conn::Down { error, not_running: true, attempt, .. } => not_running(g, error, *attempt),
            Conn::Down { error, attempt, retry_in, .. } => center(
                column![
                    icon(Icon::Unplug).size(34).style(Tone::Bad.style()),
                    heading("Cannot reach wardend").size(20),
                    text(error.as_str()),
                    small(match retry_in.as_secs() {
                        s if s >= 60 => format!("Next try in {} min (attempt {attempt}).", s / 60),
                        _ => format!("Trying again (attempt {attempt})."),
                    }),
                    small("The Connection menu at the top picks another machine."),
                ]
                .spacing(12)
                .align_x(Center)
                .max_width(640),
            )
            .into(),
            Conn::Connecting { what, .. } => center(text(format!("{what}…"))).into(),
            Conn::Connected => center(
                column![
                    icon(Icon::Boxes).size(34).style(muted),
                    heading("No apps on this machine yet").size(20),
                    text("Add one here, or with `warden start server.js --name api` in a terminal."),
                    button(labeled(Icon::Plus, "Add app"))
                        .padding([7, 14])
                        .style(look::solid(Tone::Accent))
                        .on_press(Message::OpenAdd),
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
            container(
                row![
                    icon(Icon::WifiOff).size(14),
                    text(format!("Reconnecting to wardend… {error}. What is shown is as last seen.")).size(13)
                ]
                .spacing(8)
                .align_y(Center)
            )
            .padding([6, 14])
            .width(Fill)
            .style(look::banner(Tone::Warn)),
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
    let start = button(labeled(Icon::Play, if g.starting_wardend { "Starting wardend…" } else { "Start wardend" }))
        .padding([8, 16])
        .style(look::solid(Tone::Good))
        .on_press_maybe((!g.starting_wardend).then_some(Message::StartWardend));
    center(
        column![
            icon(Icon::Power).size(36).style(muted),
            heading("wardend is not running").size(22),
            text(
                "wardend is the optional host daemon that streams every app's state and events to this window; \
                 your apps keep running without it."
            )
            .align_x(Center),
            text(error).size(13).style(muted),
            row![start, small(format!("runs `warden daemon --background` {where_}"))].spacing(12).align_y(Center),
            small(format!(
                "Or run `warden daemon --background` yourself (`warden startup` keeps it running across reboots). \
                 Trying to connect again every few seconds (attempt {attempt})."
            )),
        ]
        .spacing(14)
        .align_x(Center)
        .max_width(620),
    )
    .into()
}

// ---------------------------------------------------------------- app list

fn app_list(g: &Gui) -> Element<'_, Message> {
    let all = g.model.sorted();
    let needle = g.filter.trim().to_lowercase();
    let shown: Vec<&App> =
        all.iter().copied().filter(|a| needle.is_empty() || a.name().to_lowercase().contains(&needle)).collect();
    let running = all.iter().filter(|a| a.entry.state == AppState::Running).count();
    let attention = all.iter().filter(|a| a.is_unwell()).count();
    let mut summary = format!("{} app{} · {running} running", all.len(), if all.len() == 1 { "" } else { "s" });
    if attention > 0 {
        summary += &format!(" · {attention} need{} attention", if attention == 1 { "s" } else { "" });
    }
    let search = text_input("Filter apps", &g.filter)
        .on_input(Message::AppFilter)
        .size(13)
        .padding([6, 10])
        .style(look::input)
        .icon(text_input::Icon {
            font: look::ICONS,
            code_point: Icon::Search.glyph(),
            size: Some(14.into()),
            spacing: 8.0,
            side: text_input::Side::Left,
        });
    let mut rows = Column::new().spacing(6);
    for a in &shown {
        rows = rows.push(app_row(g, a));
    }
    if shown.is_empty() {
        rows = rows.push(container(small(format!("No app matches “{}”.", g.filter.trim()))).padding([10, 8]));
    }
    container(
        column![
            column![search, small(summary)].spacing(8).padding([12, 12]),
            scrollable(rows.padding([0, 8]).width(Fill))
                .direction(scrollable::Direction::Vertical(thin()))
                .height(Fill)
                .style(look::scroll),
        ]
        .spacing(0),
    )
    .width(LIST_W)
    .height(Fill)
    .style(look::sidebar)
    .into()
}

/// The state of an app at the right of its row: a pill when it is in trouble (the
/// sentence under it is plain, so the state is the one red thing to find), else a word.
fn state_label<'a>(a: &App, tone: Tone) -> Element<'a, Message> {
    if matches!(a.entry.state, AppState::GaveUp | AppState::Unreachable) {
        badge(a.state_label(), tone)
    } else {
        text(a.state_label()).size(SMALL).font(MEDIUM).style(tone.style()).into()
    }
}

fn app_row<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let selected = g.selected.as_deref() == Some(a.name());
    let tone = state_tone(a);
    let mut parts: Vec<String> = Vec::new();
    parts.extend(a.ports_short());
    parts.extend(a.workers().map(|(r, c)| format!("{r}/{c} ready")));
    parts.extend(a.cpu_percent().map(format::percent));
    parts.extend(a.rss_bytes().map(format::bytes));
    if a.entry.supervised_by != "wardend" {
        parts.push(a.entry.supervised_by.clone());
    }
    let mut c = column![
        row![text(a.name()).size(14).font(DISPLAY_BOLD), space::horizontal(), state_label(a, tone)].align_y(Center)
    ]
    .spacing(2);
    if !parts.is_empty() {
        c = c.push(small(parts.join(" · ")));
    }
    // A failing app says why; a stopped one needs no sentence in the list.
    if let (AppState::GaveUp | AppState::Unreachable, Some(p)) = (a.entry.state, &a.entry.problem) {
        c = c.push(small(clip(p, 80).into_owned()));
    }
    // What needs attention is washed with its color (the list is read for it).
    let attention = match a.entry.state {
        AppState::GaveUp | AppState::Unreachable => Some(Tone::Bad),
        AppState::Starting => Some(Tone::Warn),
        AppState::Running if a.is_unwell() => Some(Tone::Warn),
        _ => None,
    };
    let mut inner = Row::new().spacing(10);
    if selected {
        inner = inner.push(look::marker());
    }
    let inner = inner.push(container(dot(tone, 8.0)).padding([6, 0])).push(c.width(Fill));
    button(inner)
        .width(Fill)
        .padding([10, 12])
        .style(look::list_row(selected, attention))
        .on_press(Message::Select(a.name().to_string()))
        .into()
}

// ------------------------------------------------------------------ detail

fn detail<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let e = &a.entry;
    let tone = state_tone(a);
    let mut head =
        row![heading(a.name()).size(28), badge(a.state_label(), tone), space::horizontal()].spacing(12).align_y(Center);
    if let Some(cfg) = &e.config {
        head = head.push(tip(small(clip(cfg, 52).into_owned()), cfg.clone())).push(
            button(labeled(Icon::Edit, "Edit config"))
                .padding([7, 14])
                .style(look::quiet)
                .on_press(Message::OpenEditor(e.name.clone())),
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
        if let Some(u) = &s.user {
            facts.push(format!("user {u}"));
        }
        facts.push(format!("up {}", format::duration(s.uptime_secs)));
        if let Some(r) = s.supervisor_rss_bytes {
            facts.push(format!("supervisor {}", format::bytes(r)));
        }
        if s.health_suspended {
            facts.push("health replacements suspended".into());
        }
    }
    let mut c = column![head, small(facts.join(" · "))].spacing(8);
    if let (AppState::GaveUp | AppState::Unreachable, Some(p)) = (e.state, &e.problem) {
        c = c.push(
            container(
                row![icon(Icon::AlertCircle).size(16).style(Tone::Bad.style()), text(p.as_str()).size(13)]
                    .spacing(10)
                    .align_y(Center),
            )
            .padding([9, 12])
            .width(Fill)
            .style(look::banner(Tone::Bad)),
        );
    }
    // What sits above the bottom pane, in pixels (about; the page only needs to know
    // whether the pane keeps a usable height).
    let mut top = TOPBAR_H + PAGE_PAD + HEAD_H + 8.0 + FACTS_H + GAP;
    if matches!(e.state, AppState::GaveUp | AppState::Unreachable) && e.problem.is_some() {
        top += BANNER_H + 8.0;
    }
    if a.status.is_none() {
        c = c.push(idle_card(g, a));
        top += IDLE_H + GAP;
    } else {
        c = c.push(tiles(g, a));
        top += if g.window.width >= TILES_ONE_ROW_W { TILE_H } else { 2.0 * TILE_H + 10.0 } + GAP;
        if let Some(p) = ports_row(a) {
            c = c.push(p);
            top += PORTS_H + GAP;
        }
        c = c.push(actions(g, a));
        top += ACTIONS_H + GAP;
        if let Some(r) = rollout(a) {
            c = c.push(r);
            top += ROLLOUT_H + GAP;
        }
        c = c.push(workers(g, a));
        let rows = a.status.as_ref().map_or(0, |s| s.workers.len() + s.draining.len() + s.standbys.len());
        top += if rows == 0 { 20.0 } else { table_height(g, rows) + 2.0 } + GAP;
    }
    let tab = |i: Icon, name: &'static str, t: Tab| {
        button(labeled(i, name)).padding([6, 16]).style(look::tab(g.tab == t)).on_press(Message::Tab(t))
    };
    c = c.push(
        container(
            row![
                tab(Icon::Activity, "Events", Tab::Events),
                tab(Icon::Logs, "Logs", Tab::Logs),
                tab(Icon::History, "History", Tab::History)
            ]
            .spacing(2),
        )
        .padding(4)
        .style(look::track),
    );
    top += TABS_H + GAP;
    let pane = match g.tab {
        Tab::Events => events(a, g.feed_scroll),
        Tab::Logs => logs_pane(g),
        Tab::History => history_pane(g),
    };
    if g.window.height - top - PAGE_PAD >= PANE_MIN_H {
        return c.push(pane).spacing(14).padding([18, 22]).width(Fill).height(Fill).into();
    }
    // Too short for the pane to keep its height: the page scrolls instead, and the
    // pane has a height of its own.
    let pane_h = if g.tab == Tab::History { 460.0 } else { 220.0 };
    scrollable(c.push(container(pane).height(pane_h)).spacing(14).padding([18, 22]).width(Fill))
        .style(look::scroll)
        .direction(scrollable::Direction::Vertical(thin()))
        .width(Fill)
        .height(Fill)
        .into()
}

/// An app with no supervisor to talk to: what it is, and how to start it.
fn idle_card<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let (title, what) = match a.entry.state {
        AppState::NotStarted => ("Not started", "It is configured, and has no supervisor yet."),
        AppState::Stopped => ("Stopped", "Its supervisor is idle: no worker runs."),
        AppState::GaveUp => ("Stopped trying", "Warden gave up restarting it; fix the cause, then start it again."),
        AppState::Unreachable => ("Cannot be reached", "Its supervisor does not answer."),
        AppState::Starting => ("Starting", "Its supervisor is starting."),
        AppState::Running => ("Waiting for its status", "wardend has not sent this app's status yet."),
    };
    let busy = g.busy.iter().any(|(n, x)| n == a.name() && *x == Act::Start);
    let start = button(labeled(Icon::Play, if busy { "Starting…" } else { "Start" }))
        .padding([8, 18])
        .style(look::solid(Tone::Good))
        .on_press_maybe((g.connected() && !busy).then(|| Message::Act(a.name().to_string(), Act::Start)));
    let mut c = column![heading(title).size(16), text(what).size(13)].spacing(4);
    if let (AppState::NotStarted | AppState::Stopped, Some(p)) = (a.entry.state, &a.entry.problem) {
        c = c.push(small(p.clone()));
    }
    container(row![icon(Icon::Power).size(30).style(muted), c.width(Fill), start].spacing(18).align_y(Center))
        .padding([18, 20])
        .width(Fill)
        .style(look::card)
        .into()
}

/// Under this window width the five tiles do not fit in a row.
const TILES_ONE_ROW_W: f32 = 1100.0;
/// A tile's height (two lines and the padding).
const TILE_H: f32 = 98.0;

fn tiles<'a>(g: &Gui, a: &'a App) -> Element<'a, Message> {
    let Some(s) = &a.status else { return space().into() };
    let (ready, configured) = (s.workers_ready, s.workers_configured);
    let (workers_sub, workers_tone) = match (ready, configured) {
        (_, 0) => ("scaled to 0".to_string(), Tone::Muted),
        (r, c) if r == c => ("all ready".to_string(), Tone::Good),
        (r, c) => (format!("{} not ready", c - r), Tone::Warn),
    };
    let crashes: u64 = s.workers.iter().map(|w| w.crashes).sum();
    let all: Vec<Element<'a, Message>> = vec![
        tile(Icon::Workers, "Workers", format!("{ready} / {configured}"), workers_sub, workers_tone),
        tile(Icon::Cpu, "CPU", format::opt(a.cpu_percent(), format::percent), "all its processes".into(), Tone::Plain),
        tile(
            Icon::Memory,
            "Memory",
            format::opt(a.rss_bytes(), format::bytes),
            "workers + supervisor".into(),
            Tone::Plain,
        ),
        tile(
            Icon::Clock,
            "Uptime",
            format::duration(s.uptime_secs),
            s.user.as_ref().map_or_else(String::new, |u| format!("as {u}")),
            Tone::Plain,
        ),
        tile(
            Icon::Restart,
            "Restarts",
            a.restarts().to_string(),
            if crashes == 0 {
                "no crashes".into()
            } else {
                format!("{crashes} crash{}", if crashes == 1 { "" } else { "es" })
            },
            if crashes == 0 { Tone::Plain } else { Tone::Warn },
        ),
    ];
    if g.window.width >= TILES_ONE_ROW_W {
        return Row::with_children(all).spacing(10).into();
    }
    // Three and two: the last row keeps the tiles' width with an empty slot.
    let mut all = all.into_iter();
    let first = Row::with_children(all.by_ref().take(3)).spacing(10);
    let second = Row::with_children(
        all.chain(std::iter::once(space().width(Length::FillPortion(1)).into())).collect::<Vec<_>>(),
    )
    .spacing(10);
    column![first, second].spacing(10).into()
}

/// The ports and sockets the app listens on; a click copies the address.
fn ports_row(a: &App) -> Option<Element<'_, Message>> {
    let chips = a.ports();
    let live = a.status.as_ref().is_some_and(|s| !s.stopped && s.workers.iter().any(|w| w.pid.is_some()));
    if chips.is_empty() && !live {
        return None;
    }
    let mut r = Row::new().spacing(8).align_y(Center);
    r = r.push(row![icon(Icon::Network).size(14).style(muted), section("Listening")].spacing(6).align_y(Center));
    if chips.is_empty() {
        r = r.push(small("nothing yet (a port shows a second or two after the app binds it)"));
    }
    for c in chips {
        let body = row![
            if c.unix { icon(Icon::Plug).size(13).style(muted) } else { icon(Icon::Link).size(13).style(muted) },
            text(c.label.clone()).font(MONO).size(13),
            small(if c.unix { "unix socket".to_string() } else { c.scope.clone() }),
        ]
        .spacing(6)
        .align_y(Center);
        let hint = if c.unix { format!("Copy the path {}", c.copy) } else { format!("Copy {}", c.copy) };
        r = r.push(tip(button(body).padding([3, 10]).style(look::chip).on_press(Message::Copy(c.copy.clone())), hint));
    }
    Some(r.wrap().into())
}

fn actions<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let up = g.connected() && a.supervisor_up();
    let stopped = a.status.as_ref().is_some_and(|s| s.stopped);
    let go = up && !stopped;
    let busy = |act: Act| g.busy.iter().any(|(n, x)| n == a.name() && *x == act);
    let name = a.name().to_string();
    let press = |act: Act| (!busy(act)).then(|| Message::Act(name.clone(), act));

    // Restart | ▾: the main click is the safe reload; the menu has the others.
    let restarting = [Act::SafeReload, Act::Reload, Act::RollingRestart, Act::HardRestart].into_iter().any(busy);
    let open = g.menu == Some(MenuKind::Restart) && go;
    let main =
        button(container(labeled(Icon::Restart, if restarting { "Restarting…" } else { "Restart" })).center_y(32))
            .height(32)
            .padding([0, 14])
            .style(look::split_left)
            .on_press_maybe((go && !restarting).then(|| Message::Act(name.clone(), Act::SafeReload)));
    let arrow = button(container(icon(if open { Icon::ChevronUp } else { Icon::ChevronDown }).size(14)).center_y(Fill))
        .height(32)
        .padding([0, 8])
        .style(look::split_right)
        .on_press_maybe(go.then_some(Message::ToggleMenu(MenuKind::Restart)));
    let split = row![main, look::divider(Tone::Accent), arrow].height(32);
    let menu = container(
        column![
            menu_item(
                Some(Icon::SafeReload),
                Tone::Accent,
                "Safe reload".into(),
                Some("The default: preflight, a canary soak, rollback on failure".into()),
                press(Act::SafeReload),
            ),
            menu_item(
                Some(Icon::Reload),
                Tone::Plain,
                "Reload".into(),
                Some("One worker at a time, each through the health gates".into()),
                press(Act::Reload),
            ),
            menu_item(
                Some(Icon::Rolling),
                Tone::Plain,
                "Rolling restart".into(),
                Some("Restart every worker in turn, with no downtime".into()),
                press(Act::RollingRestart),
            ),
            rule::horizontal(1),
            menu_item(
                Some(Icon::Hard),
                Tone::Bad,
                "Hard restart".into(),
                Some("Every worker at once: the app is down meanwhile".into()),
                press(Act::HardRestart),
            ),
        ]
        .spacing(2),
    )
    .width(360)
    .padding(4)
    .style(look::menu);
    let restart = Dropdown::new(split, menu, open).on_dismiss(Message::CloseMenu);

    let count = a.status.as_ref().map(|s| s.workers_configured).unwrap_or(0);
    let step = |i: Icon, to: usize, enabled: bool| {
        let act = Act::Scale(to);
        button(container(icon(i).size(14)).center_x(Fill).center_y(Fill))
            .width(32)
            .height(32)
            .style(look::quiet)
            .on_press_maybe((enabled && !busy(act)).then(|| Message::Act(name.clone(), act)))
    };
    let scale = row![
        step(Icon::Minus, count.saturating_sub(1), up && count > 0),
        container(text(format!("{count} workers")).size(13)).padding([0, 12]).center_y(32),
        step(Icon::Plus, count + 1, up),
    ]
    .align_y(Center);

    let start_ok = g.connected() && (!a.supervisor_up() || stopped);
    let mut r = Row::new().spacing(8).align_y(Center);
    if start_ok {
        r = r.push(
            button(container(labeled(Icon::Play, if busy(Act::Start) { "Starting…" } else { "Start" })).center_y(32))
                .height(32)
                .padding([0, 14])
                .style(look::solid(Tone::Good))
                .on_press_maybe(press(Act::Start)),
        );
    }
    if go {
        r = r.push(restart);
        r = r.push(
            button(container(labeled(Icon::Stop, if busy(Act::Stop) { "Stopping…" } else { "Stop" })).center_y(32))
                .height(32)
                .padding([0, 12])
                .style(look::quiet_tone(Tone::Bad))
                .on_press_maybe(press(Act::Stop)),
        );
    }
    if up {
        r = r.push(tip(
            button(container(labeled(Icon::Reset, "Reset")).center_y(32))
                .height(32)
                .padding([0, 12])
                .style(look::quiet)
                .on_press_maybe(press(Act::Reset)),
            "Clear crash counters and the failed state",
        ));
    }
    row![r, space::horizontal(), scale].spacing(12).align_y(Center).into()
}

fn rollout(a: &App) -> Option<Element<'_, Message>> {
    let mut c = column![].spacing(6);
    if let Some(r) = &a.rollout {
        let total = r.total.max(1) as f32;
        c = c.push(
            row![
                icon(Icon::Loader).size(14).style(Tone::Accent.style()),
                text(format!("{} {}/{}: {} ({})", r.kind, r.done, r.total, r.phase, format::duration(r.elapsed_secs)))
                    .size(13),
            ]
            .spacing(8)
            .align_y(Center),
        );
        c = c.push(progress_bar(0.0..=total, r.done as f32).girth(6).style(look::progress));
    }
    if let Some(o) = &a.last_outcome {
        let t = row![
            icon(if o.ok { Icon::CheckCircle } else { Icon::AlertCircle }).size(14).style(if o.ok {
                Tone::Good.style()
            } else {
                Tone::Bad.style()
            }),
            text(format!(
                "Last {}: {} ({:.1} s): {}",
                o.kind,
                if o.ok { "ok" } else { "FAILED" },
                o.duration_secs,
                o.message
            ))
            .size(13)
            .style({
                let ok = o.ok;
                move |theme: &Theme| if ok { muted(theme) } else { Tone::Bad.style()(theme) }
            }),
        ]
        .spacing(8)
        .align_y(Center);
        c = c.push(t);
    }
    (a.rollout.is_some() || a.last_outcome.is_some())
        .then(|| container(c).padding([10, 14]).width(Fill).style(look::card).into())
}

/// What a row of the worker table is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Worker,
    /// An old process a rollout replaced, draining until it exits.
    Draining,
    /// A hot standby.
    Standby,
}

/// A row of the worker table: (what it is, its status).
type WorkerRow<'a> = (RowKind, &'a WorkerStatus);

/// Every cell of the worker table is this tall, so the table's height is known
/// before it is laid out (the card around it is as tall as its rows, up to
/// `TABLE_MAX_H`).
const CELL_H: f32 = 32.0;
const TABLE_MAX_H: f32 = 270.0;
/// What the sideways scrollbar takes from the bottom of the card.
const SCROLLBAR_H: f32 = 12.0;
/// The table is at least this wide; under it, the card scrolls sideways.
const TABLE_MIN_W: f32 = 880.0;

/// The height of the table's content: its rows and the header, and the sideways
/// scrollbar when the card is narrower than the table.
fn table_height(g: &Gui, rows: usize) -> f32 {
    let lines = rows + 1; // and the header
    // The card is the window less the app list, the page's padding and the card's border.
    let scrolls_sideways = g.window.width - LIST_W - 1.0 - 32.0 - 2.0 < TABLE_MIN_W;
    (lines as f32 * CELL_H + (lines - 1) as f32).min(TABLE_MAX_H) + if scrolls_sideways { SCROLLBAR_H } else { 0.0 }
}

fn workers<'a>(g: &'a Gui, a: &'a App) -> Element<'a, Message> {
    let Some(s) = &a.status else { return space().into() };
    if s.workers.is_empty() {
        return text("No workers (scaled to 0).").size(13).style(muted).into();
    }
    // Workers, old processes still draining after a rollout
    // (`Status.draining`), then hot standbys (`Status.standbys`).
    let rows: Vec<WorkerRow<'a>> = s
        .workers
        .iter()
        .map(|w| (RowKind::Worker, w))
        .chain(s.draining.iter().map(|w| (RowKind::Draining, w)))
        .chain(s.standbys.iter().map(|w| (RowKind::Standby, w)))
        .collect();
    let height = table_height(g, rows.len());
    let can_restart = g.connected() && a.supervisor_up() && !s.stopped;
    let worker_mode = s.mode == "worker";
    let app = a.name().to_string();
    // A column nothing fills in (no loop delay without the shim, no health check, no
    // exit yet) is left out rather than shown as a row of dashes.
    let any = |f: fn(&WorkerStatus) -> bool| rows.iter().any(|(_, w)| f(w));
    let (has_loop, has_health, has_exit) =
        (any(|w| w.loop_delay.is_some()), any(|w| w.healthy.is_some()), any(|w| w.last_exit.is_some()));
    container(responsive(move |size| {
        // The table is as wide as the card, so its lines run from border to border.
        let width = size.width.max(TABLE_MIN_W);
        let rows = rows.clone();
        let cell_box =
            |e: Element<'a, Message>| -> Element<'a, Message> { container(e).height(CELL_H).center_y(CELL_H).into() };
        let h = move |t: &'static str| -> Element<'a, Message> {
            container(text(t).size(11).font(DISPLAY).style(muted)).height(CELL_H).center_y(CELL_H).into()
        };
        let plain = move |t: String| -> Element<'a, Message> { cell_box(text(t).size(13).into()) };
        let mono = move |t: String| -> Element<'a, Message> { cell_box(text(t).font(MONO).size(12).into()) };
        let mut columns = vec![
            table::column(h("Worker"), move |(kind, w): WorkerRow<'a>| {
                plain(match kind {
                    RowKind::Worker => w.id.to_string(),
                    RowKind::Draining if worker_mode => "host (old)".into(),
                    RowKind::Draining => format!("{} (old)", w.id),
                    RowKind::Standby => format!("s{} (standby)", w.id),
                })
            }),
            table::column(h("State"), move |(_, w): WorkerRow<'a>| {
                let tone = match w.state.as_str() {
                    "RUNNING" | "STANDBY" => Tone::Good,
                    "FAILED" | "CRASHED" => Tone::Bad,
                    _ => Tone::Warn,
                };
                cell_box(state_pill(w.state.clone(), tone))
            }),
            table::column(h("PID"), move |(_, w): WorkerRow<'a>| mono(format::opt(w.pid, |p| p.to_string()))),
            table::column(h("Ports"), move |(_, w): WorkerRow<'a>| mono(format::ports_cell(&w.listening))),
            table::column(h("Uptime"), move |(_, w): WorkerRow<'a>| {
                plain(format::opt(w.uptime_secs, format::duration))
            })
            .align_x(iced::alignment::Horizontal::Right),
            table::column(h("Restarts"), move |(_, w): WorkerRow<'a>| plain(w.restarts.to_string()))
                .align_x(iced::alignment::Horizontal::Right),
            table::column(h("CPU"), move |(_, w): WorkerRow<'a>| plain(format::opt(w.cpu_percent, format::percent)))
                .align_x(iced::alignment::Horizontal::Right),
            table::column(h("RSS"), move |(_, w): WorkerRow<'a>| plain(format::opt(w.rss_bytes, format::bytes)))
                .align_x(iced::alignment::Horizontal::Right),
        ];
        if has_loop {
            // The event-loop delay's p99 over the last second, as `warden list` shows it.
            columns.push(
                table::column(h("Loop p99"), move |(_, w): WorkerRow<'a>| {
                    plain(format::opt(w.loop_delay.map(|d| d.p99_ms), format::millis))
                })
                .align_x(iced::alignment::Horizontal::Right),
            );
        }
        if has_health {
            columns.push(table::column(h("Health"), move |(_, w): WorkerRow<'a>| {
                let t = text(format::health(w.healthy).to_string()).size(13);
                cell_box(match w.healthy {
                    Some(false) => t.style(Tone::Bad.style()).into(),
                    _ => t.into(),
                })
            }));
        }
        if has_exit {
            columns.push(table::column(h("Last exit"), move |(_, w): WorkerRow<'a>| {
                plain(w.last_exit.clone().unwrap_or_else(|| "-".into()))
            }));
        }
        // The last column takes what is left, so the table is as wide as its card; a
        // standby is not restarted on its own (it is replaced when it fails), nor is an
        // old process that drains (it is on its way out).
        let app = app.clone();
        columns.push(
            table::column(container(text("")).width(Fill).height(CELL_H), move |(kind, w): WorkerRow<'a>| {
                let id = w.id;
                // A cell that fills (not just a column that does) is what makes the table use the room.
                container(tip(
                    button(icon(Icon::Restart).size(13)).padding([3, 7]).style(look::ghost).on_press_maybe(
                        (can_restart && kind == RowKind::Worker)
                            .then(|| Message::Act(app.clone(), Act::RestartWorker(id))),
                    ),
                    format!("Restart worker {id}"),
                ))
                .width(Fill)
                .height(CELL_H)
                .center_y(CELL_H)
                .align_x(iced::alignment::Horizontal::Right)
            })
            .width(Fill),
        );
        let t = table(columns, rows).width(width).padding_x(18).padding_y(0).separator_x(0).separator_y(1);
        scrollable(t)
            .style(look::scroll)
            .direction(scrollable::Direction::Both { vertical: thin(), horizontal: thin() })
            .width(Fill)
            .height(Fill)
            .into()
    }))
    .height(height + 2.0)
    .width(Fill)
    .style(look::card)
    .into()
}

// ---------------------------------------------------------- events & logs

/// One row of a list.
struct Line<'a> {
    time: Option<&'a str>,
    text: &'a str,
    tone: Tone,
    /// A dot before the text (events), colored by what the line is about.
    dot: Option<Tone>,
    mono: bool,
}

/// A list drawn only where it is visible: every row is `LINE_H` tall, the
/// rest is spacers. Anchored at the bottom, like a terminal. A line wider
/// than the list is cut with `…` and shows in full in a tooltip.
fn lines<'a>(
    items: Vec<Line<'a>>,
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
        for item in &items[start..end] {
            let char_w = if item.mono { 7.4 } else { 6.9 };
            let used =
                16.0 + if item.time.is_some() { 68.0 } else { 0.0 } + if item.dot.is_some() { 16.0 } else { 0.0 };
            let fit = (((size.width - used - 14.0) / char_w) as usize).max(12);
            let shown = clip(item.text, fit);
            let cut = matches!(shown, Cow::Owned(_));
            let mut r = Row::new().spacing(8).align_y(Center).height(LINE_H);
            if let Some(t) = item.time {
                r = r.push(
                    text(t).font(MONO).size(11).style(muted).width(60).line_height(LineHeight::Absolute(LINE_H.into())),
                );
            }
            if let Some(d) = item.dot {
                r = r.push(dot(d, 6.0));
            }
            let body = text(shown.into_owned())
                .size(if item.mono { 12 } else { 13 })
                .line_height(LineHeight::Absolute(LINE_H.into()))
                .wrapping(Wrapping::None)
                .style(item.tone.style());
            r = r.push(if item.mono { body.font(MONO) } else { body });
            col = col.push(if cut { tip(r, item.text.to_string()) } else { r.into() });
        }
        if end < total {
            col = col.push(space().height((total - end) as f32 * LINE_H));
        }
        scrollable(col.padding([5, 10]))
            .style(look::scroll)
            .direction(scrollable::Direction::Vertical(thin()))
            .id(Id::new(id))
            .anchor_bottom()
            .width(Fill)
            .height(Fill)
            .on_scroll(move |v| on_scroll(v.absolute_offset().y))
            .into()
    });
    container(list).style(look::card).width(Fill).height(Fill).into()
}

fn events(a: &App, scroll: Scroll) -> Element<'_, Message> {
    if a.feed.is_empty() {
        return container(small("No events yet.")).padding(10).into();
    }
    let items = a
        .feed
        .iter()
        .map(|l| {
            let (time, text) = format::split_event(l, a.name());
            let sev = format::severity(text);
            Line {
                time,
                text,
                tone: if matches!(sev, Severity::Bad | Severity::Warn) { severity_tone(sev) } else { Tone::Plain },
                dot: Some(severity_tone(sev)),
                mono: false,
            }
        })
        .collect();
    lines(items, scroll, FEED_ID, Message::FeedScrolled)
}

fn logs_pane(g: &Gui) -> Element<'_, Message> {
    let Some(p) = &g.logs else { return space().into() };
    let pause = if p.paused {
        button(labeled(Icon::Play, format!("Resume ({} new)", p.held())))
            .padding([4, 10])
            .style(look::solid(Tone::Accent))
            .on_press(Message::LogsPause(false))
    } else {
        button(labeled(Icon::Pause, "Pause")).padding([4, 10]).style(look::quiet).on_press(Message::LogsPause(true))
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
        checkbox(p.stdout).label("stdout").on_toggle(Message::LogsStdout).size(14).text_size(13).style(look::check),
        checkbox(p.stderr).label("stderr").on_toggle(Message::LogsStderr).size(14).text_size(13).style(look::check),
        checkbox(p.events).label("events").on_toggle(Message::LogsEvents).size(14).text_size(13).style(look::check),
        text_input("Filter lines", &p.filter)
            .on_input(Message::LogsFilter)
            .size(13)
            .width(240)
            .padding([5, 10])
            .style(look::input)
            .icon(text_input::Icon {
                font: look::ICONS,
                code_point: Icon::Search.glyph(),
                size: Some(14.into()),
                spacing: 8.0,
                side: text_input::Side::Left,
            }),
        pause,
        button(labeled(Icon::Close, "Clear")).padding([4, 10]).style(look::quiet).on_press(Message::LogsClear),
        small(notes.join(" · ")),
    ]
    .spacing(12)
    .align_y(Center);
    let visible = p.visible();
    let body = if visible.is_empty() {
        container(small(if p.is_empty() { "No log lines yet." } else { "No line matches." }))
            .padding(10)
            .height(Fill)
            .into()
    } else {
        let items = visible
            .iter()
            .map(|l| Line {
                time: None,
                text: l.text.as_str(),
                tone: match l.stream {
                    Stream::Stderr => Tone::Bad,
                    Stream::Note => Tone::Warn,
                    Stream::Event => Tone::Muted,
                    Stream::Stdout => Tone::Plain,
                },
                dot: None,
                mono: true,
            })
            .collect();
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
/// The History tab: four charts, as tall as the room allows but never under
/// `CHART_MIN_H` (a shorter window scrolls instead of squashing the axes).
fn history_pane(g: &Gui) -> Element<'_, Message> {
    responsive(move |size| {
        let row_h = ((size.height - 52.0) / 2.0).max(CHART_MIN_H);
        scrollable(history_content(g, row_h)).style(look::scroll).width(Fill).height(Fill).into()
    })
    .into()
}

const CHART_MIN_H: f32 = 150.0;

fn history_content(g: &Gui, row_h: f32) -> Element<'_, Message> {
    let Some(c) = &g.chart else { return space().into() };
    let ranges = container(
        Row::with_children(Range::ALL.map(|r| {
            button(text(r.label()).size(13).font(SEMIBOLD))
                .style(look::tab(c.range == r))
                .padding([4, 14])
                .on_press(Message::HistoryRange(r))
                .into()
        }))
        .spacing(2),
    )
    .padding(3)
    .style(look::track);
    let per_point = match c.grid.step_s {
        s if s < 60 => format!("{s} s per point"),
        s => format!("{} min per point", s / 60),
    };
    let note: Element<'_, Message> = match &c.load {
        Load::Loading => small("loading…").into(),
        Load::Failed(e) => text(e.as_str()).size(SMALL).style(Tone::Bad.style()).into(),
        Load::Ready => small(format!("{per_point} · from wardend, then live")).into(),
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
    let card =
        |title: &'static str, sub: &'static str, summary: String, i: usize, unit: Unit, hue: Hue, look_: Look| {
            let chart = Chart {
                series: &s[i],
                start_s: c.grid.start_s,
                step_s: c.grid.step_s,
                end_s: if c.grid.is_empty() { warden_protocol::events::now_ms() / 1000 } else { c.grid.end_s() },
                span_s: span,
                unit,
                hue,
                bars: look_ == Look::Bars,
                area: look_ == Look::Area,
                faded: c.load == Load::Loading,
            };
            container(
                column![
                    row![text(title).size(14).font(SEMIBOLD), small(sub), space::horizontal(), small(summary)]
                        .spacing(8)
                        .align_y(Center),
                    canvas(chart).width(Fill).height(Fill),
                ]
                .spacing(4),
            )
            .padding([10, 12])
            .width(Fill)
            .height(Fill)
            .style(look::card)
        };
    column![
        bar,
        row![
            card("CPU", "% of one core", cpu, history::CPU, Unit::Percent, Hue::Blue, Look::Area),
            card("Memory", "resident, with the supervisor", mem, history::MEM, Unit::Bytes, Hue::Aqua, Look::Area),
        ]
        .spacing(10)
        .height(row_h),
        row![
            card("Restarts", "per point", restarts, history::RESTARTS, Unit::Count, Hue::Orange, Look::Bars),
            card("Workers ready", "the fewest per point", ready, history::READY, Unit::Count, Hue::Violet, Look::Line),
        ]
        .spacing(10)
        .height(row_h),
    ]
    .spacing(10)
    .into()
}

// ----------------------------------------------------------------- dialogs

fn confirm_dialog(p: &Pending) -> Element<'_, Message> {
    column![
        row![icon(Icon::Alert).size(20).style(Tone::Warn.style()), heading(p.act.label()).size(16)]
            .spacing(10)
            .align_y(Center),
        text(p.act.question(&p.app)).size(14),
        row![
            space::horizontal(),
            button(text("Cancel").size(13).font(MEDIUM))
                .padding([6, 14])
                .style(look::quiet)
                .on_press(Message::Cancelled),
            button(text(p.act.label()).size(13).font(MEDIUM))
                .padding([6, 14])
                .style(look::solid(Tone::Bad))
                .on_press(Message::Confirmed),
        ]
        .spacing(10),
    ]
    .spacing(16)
    .width(480)
    .into()
}

fn machine_dialog(f: &MachineForm) -> Element<'_, Message> {
    let mut c = column![
        heading(if f.editing.is_some() { "Edit SSH machine" } else { "Add SSH machine" }).size(18),
        small(
            "The window opens `ssh -N -L <local socket>:<remote socket> <target>` and speaks to wardend through it. \
             It never asks for a password: ssh uses your agent and keys (BatchMode).",
        ),
        field(
            "SSH target",
            text_input("user@host (or a Host from ~/.ssh/config)", &f.dest)
                .on_input(Message::MachineDest)
                .on_submit(Message::SaveMachine)
                .padding([6, 10])
                .style(look::input),
        ),
        field(
            "wardend's socket there",
            text_input("/run/warden/wardend.sock", &f.remote_socket)
                .on_input(Message::MachineSocket)
                .padding([6, 10])
                .style(look::input),
        ),
        small(
            "wardend run by root listens on /run/warden/wardend.sock (macOS: /var/run/warden/wardend.sock); run by a \
             user, on /run/user/<uid>/warden/wardend.sock.",
        ),
        field(
            "The warden CLI there (for Add app, Edit config, Start wardend)",
            text_input("warden", &f.remote_warden).on_input(Message::MachineWarden).padding([6, 10]).style(look::input),
        ),
    ]
    .spacing(12)
    .width(560);
    if let Some(e) = &f.error {
        c = c.push(text(e.as_str()).size(13).style(Tone::Bad.style()));
    }
    c.push(
        row![
            space::horizontal(),
            button(text("Cancel").size(13).font(MEDIUM))
                .padding([6, 14])
                .style(look::quiet)
                .on_press(Message::CloseModal),
            button(text("Save and connect").size(13).font(MEDIUM))
                .padding([6, 14])
                .style(look::solid(Tone::Accent))
                .on_press(Message::SaveMachine),
        ]
        .spacing(10),
    )
    .into()
}

fn add_dialog<'a>(g: &'a Gui, a: &'a crate::app::AddForm) -> Element<'a, Message> {
    let f = &a.form;
    let running = a.status == AddStatus::Running;
    let input = |ph: &'a str, v: &'a str, m: fn(String) -> Message| {
        let t = text_input(ph, v).padding([6, 10]).style(look::input);
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
        heading("Add an app").size(18),
        small(format!("Runs `warden start` on {where_}; it waits until the app is up and says why if it is not.")),
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
        text(preview).font(MONO).size(SMALL).style(muted),
    ]
    .spacing(12)
    .width(620);
    match &a.status {
        AddStatus::Editing => {}
        AddStatus::Running => c = c.push(small("Running… (`warden start` waits until the app is up)")),
        AddStatus::Done(Err(e)) => c = c.push(text(e.as_str()).size(13).style(Tone::Bad.style())),
        AddStatus::Done(Ok(added)) => {
            let out = &added.output;
            let mut log = out.text();
            for n in &added.notes {
                log.push('\n');
                log.push_str(n);
            }
            c = c.push(
                text(if out.ok { "Done:" } else { "warden start failed:" }.to_string()).size(13).style(if out.ok {
                    Tone::Good.style()
                } else {
                    Tone::Bad.style()
                }),
            );
            c = c.push(
                container(scrollable(text(log).font(MONO).size(SMALL)).height(Length::Shrink).style(look::scroll))
                    .max_height(220)
                    .padding(8)
                    .width(Fill)
                    .style(look::card),
            );
        }
    }
    c.push(
        row![
            space::horizontal(),
            button(text("Close").size(13).font(MEDIUM))
                .padding([6, 14])
                .style(look::quiet)
                .on_press_maybe((!running).then_some(Message::CloseModal)),
            button(text(if running { "Adding…" } else { "Add" }).size(13).font(MEDIUM))
                .padding([6, 14])
                .style(look::solid(Tone::Accent))
                .on_press_maybe((!running).then_some(Message::SubmitAdd)),
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
        row![heading(format!("{}: {}", e.app, e.path)).size(15), space::horizontal(), small(status)].align_y(Center),
        text_editor(&e.content).on_action(Message::Edit).font(MONO).size(13).height(Fill).style(look::editor),
    ]
    .spacing(10)
    .width(860)
    .height(620);
    match &e.result {
        Some(Ok(m)) => c = c.push(text(format!("✓ {m}")).size(13).style(Tone::Good.style())),
        Some(Err(m)) => {
            c = c.push(
                container(scrollable(text(m.as_str()).size(13).style(Tone::Bad.style())).style(look::scroll))
                    .max_height(120)
                    .width(Fill),
            )
        }
        None => {}
    }
    let mut buttons = row![
        small("Validate runs `warden check -c` on the text; Save validates, then writes the file."),
        space::horizontal(),
        button(text("Close").size(13).font(MEDIUM))
            .padding([6, 14])
            .style(look::quiet)
            .on_press_maybe((e.status != EditorStatus::Saving).then_some(Message::CloseModal)),
        button(text("Validate").size(13).font(MEDIUM))
            .padding([6, 14])
            .style(look::quiet)
            .on_press_maybe(ready.then_some(Message::Validate)),
        button(text("Save").size(13).font(MEDIUM))
            .padding([6, 14])
            .style(look::solid(Tone::Accent))
            .on_press_maybe((ready && e.dirty).then_some(Message::Save)),
    ]
    .spacing(10)
    .align_y(Center);
    if e.saved {
        buttons = buttons.push(
            button(labeled(Icon::Reload, "Reload now"))
                .padding([6, 14])
                .style(look::solid(Tone::Good))
                .on_press(Message::ReloadAfterSave),
        );
    }
    c.push(buttons).into()
}

// ------------------------------------------------------------------ toasts

fn toasts(g: &Gui) -> Element<'_, Message> {
    let list = Column::with_children(g.toasts.iter().map(|t| {
        let tone = if t.ok { Tone::Good } else { Tone::Bad };
        container(
            row![
                icon(if t.ok { Icon::CheckCircle } else { Icon::AlertCircle }).size(16).style(tone.style()),
                text(t.text.as_str()).size(13).width(Fill),
                button(icon(Icon::Close).size(13))
                    .style(look::ghost)
                    .padding([2, 6])
                    .on_press(Message::DismissToast(t.id)),
            ]
            .spacing(10)
            .align_y(Center),
        )
        .padding([9, 12])
        .width(460)
        .style(move |theme: &Theme| {
            let mut s = look::tinted(tone, theme, 14.0);
            // Opaque enough to read over the lists.
            s.background = Some(look::pal(theme).card.into());
            s.border =
                iced::Border { radius: 14.0.into(), width: 1.0, color: iced::Color { a: 0.5, ..tone.color(theme) } };
            s
        })
        .into()
    }))
    .spacing(8);
    container(list).width(Fill).height(Fill).align_right(Fill).align_bottom(Fill).padding(16).into()
}
