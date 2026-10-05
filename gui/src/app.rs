//! The iced program: state, messages, `update` and subscriptions. The
//! window is drawn by `view`; everything slow (sockets, subprocesses) runs
//! in tasks and subscriptions on the async executor.

use crate::cli_install;
use crate::client::{self, Endpoint, FeedMsg, FeedOptions};
use crate::commands::{self, AddApp, Added, Host, Output};
use crate::history::{AppChart, HostSpark, Load, Range};
use crate::hosts::{self, Machine, Saved};
use crate::logs::{self, LogPane, Scroll};
use crate::model::Model;
use crate::ssh;
use crate::system;
use iced::widget::{Id, operation, text_editor};
use iced::{Subscription, Task};
use std::path::PathBuf;
use std::time::Duration;
use warden_protocol::control::Request;
use warden_protocol::events::{Event, ResourceHistory, now_ms};

pub const FEED_ID: &str = "feed";
pub const LOGS_ID: &str = "logs";
/// Toasts on screen at once.
const TOASTS: usize = 4;

/// Where wardend is and where commands run: always the same host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub endpoint: Endpoint,
    pub host: Host,
}

impl Target {
    /// This machine's wardend: `socket`, else the one that is running (see
    /// [`client::auto_local_socket`]).
    pub fn local(socket: Option<PathBuf>) -> Target {
        Target { endpoint: Endpoint::Socket(socket.unwrap_or_else(client::auto_local_socket)), host: Host::Local }
    }

    /// The name this machine goes by in the connection menu.
    pub fn machine_name(&self) -> String {
        match &self.endpoint {
            Endpoint::Socket(_) => "This machine".into(),
            Endpoint::Ssh(t) => t.dest.clone(),
        }
    }

    pub fn ssh(dest: &str, remote_socket: &str, remote_warden: &str) -> Result<Target, String> {
        ssh::validate_dest(dest)?;
        ssh::validate_socket_path(remote_socket, "remote")?;
        let warden =
            if remote_warden.trim().is_empty() { "warden".to_string() } else { remote_warden.trim().to_string() };
        Ok(Target {
            endpoint: Endpoint::Ssh(ssh::Target { dest: dest.to_string(), remote_socket: remote_socket.to_string() }),
            host: Host::Ssh { dest: dest.to_string(), warden },
        })
    }

    pub fn describe(&self) -> String {
        match &self.endpoint {
            Endpoint::Socket(p) => format!("this machine ({})", p.display()),
            Endpoint::Ssh(t) => format!("{} over SSH ({})", t.dest, t.remote_socket),
        }
    }
}

/// The connection to wardend, as the connection bar shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum Conn {
    Connecting { attempt: u32, what: String },
    Connected,
    Down { error: String, not_running: bool, retry_in: Duration, attempt: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Events,
    Logs,
    /// Charts of the last hours (`history` from wardend, then live).
    History,
}

/// The window as it opens (until the first resize says otherwise), and the least it can be.
pub const WINDOW: iced::Size = iced::Size::new(1280.0, 820.0);
pub const WINDOW_MIN: iced::Size = iced::Size::new(900.0, 560.0);

/// An action on an app (`DaemonRequest::App`, or wardend's `start`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Reload,
    SafeReload,
    RollingRestart,
    HardRestart,
    RestartWorker(usize),
    Scale(usize),
    Stop,
    Start,
    Reset,
}

impl Act {
    pub fn label(self) -> String {
        match self {
            Act::Reload => "Reload".into(),
            Act::SafeReload => "Safe reload".into(),
            Act::RollingRestart => "Rolling restart".into(),
            Act::HardRestart => "Hard restart".into(),
            Act::RestartWorker(w) => format!("Restart worker {w}"),
            Act::Scale(n) => format!("Scale to {n}"),
            Act::Stop => "Stop".into(),
            Act::Start => "Start".into(),
            Act::Reset => "Reset".into(),
        }
    }

    /// Stops serving: asked first.
    pub fn destructive(self) -> bool {
        matches!(self, Act::Stop | Act::HardRestart | Act::Scale(0))
    }

    /// What the confirmation asks.
    pub fn question(self, app: &str) -> String {
        match self {
            Act::Stop => format!(
                "Stop every worker of {app}? The app stops serving; its supervisor stays up and Start brings the \
                 workers back."
            ),
            Act::HardRestart => format!(
                "Hard restart {app}? Every worker stops at once, then starts again: the app is down meanwhile \
                 (Rolling restart replaces them one at a time, without downtime)."
            ),
            Act::Scale(_) => {
                format!("Scale {app} to 0 workers? Every worker stops and the app serves nothing until scaled up.")
            }
            other => format!("{} {app}?", other.label()),
        }
    }

    /// The supervisor request; `None` for Start without a running supervisor
    /// (wardend's `start` then launches it).
    pub fn request(self, supervisor_up: bool) -> Option<Request> {
        Some(match self {
            Act::Reload => Request::Reload { safe: false },
            Act::SafeReload => Request::Reload { safe: true },
            Act::RollingRestart => Request::Restart { worker: None, hard: false },
            Act::HardRestart => Request::Restart { worker: None, hard: true },
            Act::RestartWorker(w) => Request::Restart { worker: Some(w), hard: false },
            Act::Scale(n) => Request::Scale { count: n },
            Act::Stop => Request::Stop,
            Act::Start if supervisor_up => Request::Start,
            Act::Start => return None,
            Act::Reset => Request::Reset { worker: None },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub ok: bool,
    pub text: String,
}

/// The form of one SSH machine: adding it, or editing a saved one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineForm {
    /// The `dest` of the machine being edited; `None` when adding one.
    pub editing: Option<String>,
    pub dest: String,
    pub remote_socket: String,
    pub remote_warden: String,
    pub error: Option<String>,
}

/// A dropdown that is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// The app's Restart button.
    Restart,
    /// The connection button: This machine and the SSH machines.
    Machines,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddStatus {
    Editing,
    Running,
    Done(Result<Added, String>),
}

pub struct AddForm {
    pub form: AddApp,
    pub status: AddStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorStatus {
    Loading,
    Ready,
    Checking,
    Saving,
}

pub struct Editor {
    pub app: String,
    pub path: String,
    pub content: text_editor::Content,
    pub status: EditorStatus,
    /// The last check or save: warden's words.
    pub result: Option<Result<String, String>>,
    pub dirty: bool,
    /// Saved: offer to reload the app.
    pub saved: bool,
}

pub enum Modal {
    None,
    /// Add or edit an SSH machine.
    Machine(MachineForm),
    Add(AddForm),
    Editor(Editor),
    /// Colours and mode.
    Settings,
    Delete(DeleteForm),
}

/// Deleting an app: stop it and move its config to `deleted/` (`warden delete`), once its name
/// is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteForm {
    pub app: String,
    pub typed: String,
    pub running: bool,
    /// What a failed `warden delete` said.
    pub error: Option<String>,
}

impl DeleteForm {
    /// The name was typed exactly, and nothing runs yet.
    pub fn ready(&self) -> bool {
        self.typed == self.app && !self.running
    }
}

/// Settings' "Restart everything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAll {
    Idle,
    /// Asked once; waiting for "Restart now".
    Asking,
    /// `warden update --yes` is running (the window loses wardend and gets it back).
    Running,
}

/// A destructive action waiting for "yes".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub app: String,
    pub act: Act,
}

/// How the GUI starts (command-line options).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub socket: Option<PathBuf>,
    pub ssh: Option<String>,
    pub remote_socket: Option<String>,
    pub remote_warden: Option<String>,
    /// The local `warden` CLI (default: next to warden-gui, else on PATH).
    pub warden: Option<PathBuf>,
    /// What the colors follow for this run (default: `WARDEN_GUI_THEME`, else what
    /// Settings saved).
    pub theme: Option<crate::system::Source>,
}

pub struct Gui {
    pub target: Target,
    pub conn: Conn,
    /// Bumped to restart the subscription at once (after starting wardend).
    pub generation: u64,
    /// Where short requests go: set once `hello` arrived.
    pub socket: Option<PathBuf>,
    pub model: Model,
    pub selected: Option<String>,
    pub tab: Tab,
    pub feed_scroll: Scroll,
    pub logs: Option<LogPane>,
    /// The logs stream is connected (a gap is noted when it drops).
    pub logs_live: bool,
    /// The Logs tab fills the window.
    pub logs_expanded: bool,
    pub toasts: Vec<Toast>,
    next_toast: u64,
    pub confirm: Option<Pending>,
    pub modal: Modal,
    pub starting_wardend: bool,
    /// Settings' "Restart everything": asking first, then running `warden update --yes`.
    pub restart_all: RestartAll,
    /// The `warden` a restart would run (looked up when it asks); `None` while looking.
    pub restart_who: Option<Result<commands::Who, String>>,
    /// Seconds since "Restart now" (it ticks while the restart runs).
    pub restart_secs: u64,
    /// A restart that did not finish: the apps may be stopped. Shown, with the way back, until the next try.
    pub restart_note: Option<String>,
    /// What runs is "start the saved apps again", not a restart.
    pub resurrecting: bool,
    /// Counts the hosts this window has shown and never starts over (unlike `generation`, which
    /// reconnects): what a request brings back carries the one it was sent in, and a reply from
    /// another host's time is not applied to this one.
    pub epoch: u64,
    /// "Install command line tool": what `warden` is on this machine, and the action in flight
    /// (the first-run banner and Settings). Always this machine, whatever host the window shows.
    pub cli: cli_install::State,
    /// Actions in flight, per app (buttons show it).
    pub busy: Vec<(String, Act)>,
    /// The History tab's range (kept across apps).
    pub range: Range,
    /// The selected app's charts, while the History tab shows.
    pub chart: Option<AppChart>,
    /// The header's host sparklines (the last hour).
    pub host_spark: HostSpark,
    /// The open dropdown, if any.
    pub menu: Option<MenuKind>,
    /// What the app list is filtered by.
    pub filter: String,
    /// The SSH machines of the connection menu, and the file they are kept in.
    pub saved: Saved,
    pub saved_path: Option<PathBuf>,
    /// The local socket was not named (no `--socket`): look again where wardend runs
    /// when it is not found.
    pub auto_local: bool,
    /// The window's size: the top bar and the worker table fit themselves to it.
    pub window: iced::Size,
    /// What the colors follow, what the desktop says, and the theme they make.
    pub source: crate::system::Source,
    pub system: crate::system::System,
    pub theme: iced::Theme,
}

#[derive(Debug, Clone)]
pub enum Message {
    Feed(FeedMsg),
    LogFeed(FeedMsg),
    Select(String),
    Tab(Tab),
    /// Pixels from the bottom (the lists are anchored there).
    FeedScrolled(f32),
    LogsScrolled(f32),
    Act(String, Act),
    Confirmed,
    Cancelled,
    Done {
        app: String,
        act: Act,
        result: Result<String, String>,
    },
    ToastExpired(u64),
    DismissToast(u64),
    LogsPause(bool),
    LogsFilter(String),
    LogsStdout(bool),
    LogsStderr(bool),
    LogsEvents(bool),
    LogsClear,
    /// The Delete button: the dialog that asks for the app's name.
    AskDelete(String),
    DeleteTyped(String),
    ConfirmDelete,
    Deleted(Result<commands::Output, String>),
    /// The logs fill the window (true), or go back under the app (false).
    LogsExpand(bool),
    /// Follow this app's log in a terminal window (`warden logs <app>`).
    LogsTerminal,
    LogsTerminalOpened(Result<(), String>),
    LogHistory {
        app: String,
        result: Result<Vec<String>, String>,
    },
    StartWardend,
    WardendStarted(Result<Output, String>),
    /// Settings: "Restart everything…" asks, "Restart now" runs it.
    AskRestartAll,
    CancelRestartAll,
    RestartAll,
    /// The `warden` that "Restart now" would run, and its version.
    RestartWho(Result<commands::Who, String>),
    RestartedAll(commands::Restart),
    /// After a restart that did not finish: `warden resurrect`.
    ResurrectAll,
    /// A second of a restart went by.
    RestartTick,
    /// A reply to a request sent to the host of epoch `epoch` (`host` names it): applied only while
    /// that host is still the one shown.
    FromHost {
        epoch: u64,
        host: String,
        inner: Box<Message>,
    },
    /// "Install command line tool" (Settings, or the first-run banner), and its undoing.
    InstallCli,
    /// "Install for this user only": `~/.local/bin`, with no administrator prompt.
    InstallCliForMe,
    CliInstalled(Result<cli_install::Outcome, String>),
    UninstallCli,
    CliRemoved(Result<cli_install::Removed, String>),
    /// "Not now" on the banner: it does not come back.
    DismissCliBanner,
    /// Open a dropdown, or close it when it is the one that is open.
    ToggleMenu(MenuKind),
    CloseMenu,
    /// Connect to this machine's wardend.
    UseThisMachine,
    /// Connect to a saved SSH machine (by its `dest`).
    UseMachine(String),
    AddMachine,
    EditMachine(String),
    ForgetMachine(String),
    MachineDest(String),
    MachineSocket(String),
    MachineWarden(String),
    SaveMachine,
    AppFilter(String),
    /// Put this on the clipboard (a URL or a socket path).
    Copy(String),
    /// The window was resized.
    Resized(iced::Size),
    /// The desktop's look changed (GNOME told us), or was read again.
    System(system::System),
    /// Read the desktop's look again (the window was focused, or the mode changed).
    ProbeSystem,
    SystemProbed(Option<system::System>),
    /// The gear: colours and mode.
    OpenSettings,
    SetColors(system::Colors),
    SetMode(system::Mode),
    CloseModal,
    OpenAdd,
    AddWhat(String),
    AddName(String),
    AddInstances(String),
    AddPort(String),
    AddEnv(String),
    SubmitAdd,
    Added(Result<Added, String>),
    OpenEditor(String),
    EditorLoaded {
        app: String,
        result: Result<String, String>,
    },
    Edit(text_editor::Action),
    Validate,
    Validated(Result<String, String>),
    Save,
    Saved(Result<String, String>),
    ReloadAfterSave,
    HistoryRange(Range),
    HistoryLoaded {
        app: String,
        range: Range,
        result: Result<ResourceHistory, String>,
    },
    HostHistoryLoaded(Result<ResourceHistory, String>),
}

/// Stamps the reply of a request to this host (see [`Message::FromHost`]).
#[derive(Debug, Clone)]
struct Tag {
    epoch: u64,
    host: String,
}

impl Tag {
    fn wrap(&self, m: Message) -> Message {
        Message::FromHost { epoch: self.epoch, host: self.host.clone(), inner: Box::new(m) }
    }
}

impl Gui {
    pub fn new(opts: Options) -> (Gui, Task<Message>) {
        if let Some(w) = &opts.warden {
            commands::set_local_warden(w.clone());
        }
        let target = match &opts.ssh {
            Some(dest) => Target::ssh(
                dest,
                opts.remote_socket.as_deref().unwrap_or(&ssh::default_remote_socket()),
                opts.remote_warden.as_deref().unwrap_or("warden"),
            ),
            None => Ok(Target::local(opts.socket.clone())),
        };
        let (target, error) = match target {
            Ok(t) => (t, None),
            Err(e) => (Target::local(opts.socket.clone()), Some(e)),
        };
        let mut g = Gui::with_target(target);
        g.cli = cli_install::State::detect();
        g.auto_local = opts.ssh.is_none() && opts.socket.is_none();
        g.saved_path = hosts::path();
        let told_saved = g.read_saved();
        // The flag and the variable win for this run; else what Settings saved.
        g.source = opts.theme.or_else(system::Source::from_env).unwrap_or(g.saved.appearance);
        if g.source.follows_system() {
            g.system = system::detect();
        }
        g.theme = system::theme(g.source, &g.system);
        let task = match error {
            Some(e) => g.toast(false, format!("--ssh: {e}; connecting to this machine instead")),
            None => Task::none(),
        };
        (g, Task::batch([task, told_saved]))
    }

    /// Read the saved machines and settings. A file that was damaged is told
    /// (with where its original is kept), and one that cannot be read is not
    /// written over: the settings are then not saved this run.
    fn read_saved(&mut self) -> Task<Message> {
        let Some(path) = self.saved_path.clone() else { return Task::none() };
        let loaded = hosts::load_checked(&path);
        self.saved = loaded.saved;
        if !loaded.writable {
            self.saved_path = None;
        }
        match loaded.problem {
            Some(text) => self.toast(false, text),
            None => Task::none(),
        }
    }

    pub fn with_target(target: Target) -> Gui {
        Gui {
            conn: Conn::Connecting {
                attempt: 1,
                what: format!("connecting to wardend on {}", target.endpoint.describe()),
            },
            target,
            generation: 0,
            socket: None,
            model: Model::default(),
            selected: None,
            tab: Tab::Events,
            feed_scroll: Scroll::default(),
            logs: None,
            logs_live: false,
            logs_expanded: false,
            toasts: Vec::new(),
            next_toast: 0,
            confirm: None,
            modal: Modal::None,
            starting_wardend: false,
            restart_all: RestartAll::Idle,
            restart_who: None,
            restart_secs: 0,
            restart_note: None,
            resurrecting: false,
            epoch: 0,
            cli: cli_install::State::default(),
            busy: Vec::new(),
            range: Range::Hour,
            chart: None,
            host_spark: HostSpark::default(),
            menu: None,
            filter: String::new(),
            saved: Saved::default(),
            saved_path: None,
            auto_local: false,
            window: WINDOW,
            source: system::Source::default(),
            system: system::System::UNKNOWN,
            theme: system::theme(system::Source::default(), &system::System::UNKNOWN),
        }
    }

    /// Ask the desktop for its look (when the window follows it).
    fn probe_system(&self) -> Task<Message> {
        if !self.source.follows_system() {
            return Task::none();
        }
        Self::read_system()
    }

    fn read_system() -> Task<Message> {
        Task::perform(async { tokio::task::spawn_blocking(system::detect).await.ok() }, Message::SystemProbed)
    }

    /// A choice in Settings: the new theme at once, kept with the machines.
    fn set_source(&mut self, source: system::Source) -> Task<Message> {
        if source == self.source {
            return Task::none();
        }
        self.source = source;
        self.saved.appearance = source;
        self.theme = system::theme(source, &self.system);
        let save = self.persist();
        Task::batch([save, self.probe_system()])
    }

    /// What the desktop says now: a new theme when its look changed.
    fn set_system(&mut self, system: system::System) {
        if system == self.system {
            return;
        }
        self.system = system;
        if self.source.follows_system() {
            self.theme = system::theme(self.source, &system);
        }
    }

    pub fn title(&self) -> String {
        match &self.target.endpoint {
            Endpoint::Socket(_) => "Warden".into(),
            Endpoint::Ssh(t) => format!("Warden: {}", t.dest),
        }
    }

    pub fn connected(&self) -> bool {
        self.conn == Conn::Connected && self.socket.is_some()
    }

    pub fn selected_app(&self) -> Option<&crate::model::App> {
        self.selected.as_ref().and_then(|s| self.model.apps.get(s))
    }

    fn toast(&mut self, ok: bool, text: String) -> Task<Message> {
        self.next_toast += 1;
        let id = self.next_toast;
        self.toasts.push(Toast { id, ok, text });
        if self.toasts.len() > TOASTS {
            self.toasts.remove(0);
        }
        // Errors stay longer: they carry the fix.
        let d = if ok { Duration::from_secs(6) } else { Duration::from_secs(20) };
        // The timer starts on the executor (the async block runs there).
        Task::perform(async move { tokio::time::sleep(d).await }, move |_| Message::ToastExpired(id))
    }

    /// What to stamp on the reply of a request to the host shown now.
    fn tag(&self) -> Tag {
        Tag { epoch: self.epoch, host: self.target.machine_name() }
    }

    /// The runtime directory of the wardend this window is connected to: what makes the `warden`
    /// CLI act on that wardend (`WARDEN_RUNTIME_DIR`). `None` for a socket the CLI cannot be pointed at.
    pub fn wardend_dir(&self) -> Option<PathBuf> {
        match &self.target.endpoint {
            Endpoint::Socket(p) => commands::runtime_dir_of(p),
            Endpoint::Ssh(t) => commands::runtime_dir_of(std::path::Path::new(&t.remote_socket)),
        }
    }

    /// A reply from a host that is no longer the one shown. Nothing of it is applied (not the busy
    /// flags, not a reconnect, not a toast about this host); a failure the person should still hear
    /// of is told with the host's name.
    fn late(&mut self, host: &str, m: Message) -> Task<Message> {
        match m {
            Message::Done { app, act, result: Err(e) } => {
                self.toast(false, format!("{host}: {} {app} failed: {e}", act.label()))
            }
            Message::WardendStarted(Ok(out)) if !out.ok => {
                self.toast(false, format!("{host}: starting wardend failed: {}", out.text()))
            }
            Message::WardendStarted(Err(e)) => self.toast(false, format!("{host}: starting wardend failed: {e}")),
            Message::RestartedAll(r) if !r.ok => self.toast(false, format!("{host}: restart everything: {}", r.text)),
            _ => Task::none(),
        }
    }

    fn select(&mut self, name: String) -> Task<Message> {
        if self.selected.as_deref() == Some(name.as_str()) {
            return Task::none();
        }
        self.selected = Some(name);
        self.open_logs();
        let chart = self.open_chart();
        Task::batch([chart, self.show_newest()])
    }

    /// (Re)open the selected app's charts when the History tab shows, and
    /// fetch them (once connected; `Connected` fetches otherwise).
    fn open_chart(&mut self) -> Task<Message> {
        self.chart = match (&self.selected, self.tab) {
            (Some(app), Tab::History) => Some(AppChart::loading(app, self.range)),
            _ => None,
        };
        self.fetch_chart()
    }

    fn fetch_chart(&mut self) -> Task<Message> {
        let socket = self.request_socket();
        let tag = self.tag();
        let (Some(c), Ok(socket)) = (&mut self.chart, socket) else { return Task::none() };
        c.load = Load::Loading;
        let (app, range) = (c.app.clone(), c.range);
        let since = now_ms().saturating_sub(range.secs() * 1000);
        let step = range.step_s() as u32;
        let asked = app.clone();
        Task::perform(async move { client::history(&socket, &asked, since, step).await }, move |result| {
            tag.wrap(Message::HistoryLoaded { app: app.clone(), range, result })
        })
    }

    /// The header's last hour of host metrics.
    fn fetch_host(&self) -> Task<Message> {
        let Ok(socket) = self.request_socket() else { return Task::none() };
        let since = now_ms().saturating_sub(Range::Hour.secs() * 1000);
        let step = Range::Hour.step_s() as u32;
        let tag = self.tag();
        Task::perform(async move { client::history(&socket, "", since, step).await }, move |r| {
            tag.wrap(Message::HostHistoryLoaded(r))
        })
    }

    /// Show the newest lines (the lists keep their scroll offset across apps).
    fn show_newest(&mut self) -> Task<Message> {
        self.feed_scroll = Scroll::default();
        let bottom = || operation::AbsoluteOffset { x: 0.0, y: 0.0 };
        Task::batch([
            operation::scroll_to(Id::new(FEED_ID), bottom()),
            operation::scroll_to(Id::new(LOGS_ID), bottom()),
        ])
    }

    /// (Re)open the logs pane for the selected app when the Logs tab shows.
    fn open_logs(&mut self) {
        self.logs = match (&self.selected, self.tab) {
            (Some(app), Tab::Logs) => Some(LogPane::new(app)),
            _ => None,
        };
        self.logs_live = false;
    }

    /// The app still exists and can take a request: the socket to send it to.
    fn request_socket(&self) -> Result<PathBuf, String> {
        match (&self.conn, &self.socket) {
            (Conn::Connected, Some(s)) => Ok(s.clone()),
            (Conn::Down { error, .. }, _) => Err(format!("not connected to wardend: {error}")),
            _ => Err("not connected to wardend yet; try again in a moment".into()),
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::System(s) | Message::SystemProbed(Some(s)) => {
                self.set_system(s);
                Task::none()
            }
            Message::SystemProbed(None) => Task::none(),
            Message::ProbeSystem => self.probe_system(),
            Message::OpenSettings => {
                self.menu = None;
                self.modal = Modal::Settings;
                // Settings tells what the desktop is, even when the window does not follow it.
                Self::read_system()
            }
            Message::SetColors(colors) => self.set_source(system::Source { colors, ..self.source }),
            Message::SetMode(mode) => self.set_source(system::Source { mode, ..self.source }),
            Message::Feed(m) => self.on_feed(m),
            Message::LogFeed(m) => self.on_log_feed(m),
            Message::Select(name) => {
                self.menu = None;
                self.select(name)
            }
            Message::Tab(tab) => {
                if self.tab == tab {
                    return Task::none();
                }
                self.tab = tab;
                self.open_logs();
                let chart = self.open_chart();
                Task::batch([chart, self.show_newest()])
            }
            Message::HistoryRange(range) => {
                self.range = range;
                match &mut self.chart {
                    // The charts shown so far stay (faded) until the new range arrives.
                    Some(c) if c.range != range => {
                        c.range = range;
                        self.fetch_chart()
                    }
                    _ => Task::none(),
                }
            }
            Message::HistoryLoaded { app, range, result } => {
                let Some(c) = self.chart.as_mut().filter(|c| c.app == app && c.range == range) else {
                    return Task::none();
                };
                match result {
                    Ok(h) => c.loaded(h),
                    Err(e) => c.load = Load::Failed(e),
                }
                Task::none()
            }
            Message::HostHistoryLoaded(result) => {
                // Without it (an older wardend) the sparklines fill from live events.
                if let Ok(h) = result {
                    self.host_spark.loaded(h);
                }
                Task::none()
            }
            Message::FeedScrolled(y) => {
                self.feed_scroll.scrolled(y);
                Task::none()
            }
            Message::LogsScrolled(y) => {
                if let Some(p) = &mut self.logs {
                    p.scroll.scrolled(y);
                }
                Task::none()
            }
            Message::Act(app, act) => {
                self.menu = None;
                if act.destructive() {
                    self.confirm = Some(Pending { app, act });
                    return Task::none();
                }
                self.act(app, act)
            }
            Message::Confirmed => match self.confirm.take() {
                Some(p) => self.act(p.app, p.act),
                None => Task::none(),
            },
            Message::Cancelled => {
                self.confirm = None;
                Task::none()
            }
            Message::Done { app, act, result } => {
                self.busy.retain(|(a, x)| !(a == &app && *x == act));
                match result {
                    Ok(msg) => self.toast(true, format!("{app}: {msg}")),
                    Err(e) => self.toast(false, format!("{} {app} failed: {e}", act.label())),
                }
            }
            Message::ToastExpired(id) | Message::DismissToast(id) => {
                self.toasts.retain(|t| t.id != id);
                Task::none()
            }
            Message::LogsPause(p) => {
                if let Some(l) = &mut self.logs {
                    l.set_paused(p);
                }
                Task::none()
            }
            Message::LogsFilter(f) => self.with_logs(|l| l.filter = f),
            Message::LogsStdout(b) => self.with_logs(|l| l.stdout = b),
            Message::LogsStderr(b) => self.with_logs(|l| l.stderr = b),
            Message::LogsEvents(b) => self.with_logs(|l| l.events = b),
            Message::LogsClear => self.with_logs(LogPane::clear),
            Message::AskDelete(app) => {
                self.menu = None;
                self.modal = Modal::Delete(DeleteForm { app, typed: String::new(), running: false, error: None });
                Task::none()
            }
            Message::DeleteTyped(t) => {
                if let Modal::Delete(d) = &mut self.modal
                    && !d.running
                {
                    d.typed = t;
                }
                Task::none()
            }
            Message::ConfirmDelete => {
                let Modal::Delete(d) = &mut self.modal else { return Task::none() };
                if !d.ready() {
                    return Task::none();
                }
                d.running = true;
                d.error = None;
                let (host, app, dir, tag) = (self.target.host.clone(), d.app.clone(), self.wardend_dir(), self.tag());
                Task::perform(async move { commands::delete_app(&host, &app, dir.as_deref()).await }, move |r| {
                    tag.wrap(Message::Deleted(r))
                })
            }
            Message::Deleted(r) => {
                let Modal::Delete(d) = &mut self.modal else { return Task::none() };
                d.running = false;
                match r {
                    Ok(out) if out.ok => {
                        let app = d.app.clone();
                        self.modal = Modal::None;
                        self.toast(true, format!("{app} deleted; its config is in the deleted folder"))
                    }
                    Ok(out) => {
                        d.error = Some(format!("{} (`{}`)", out.text(), out.command));
                        Task::none()
                    }
                    Err(e) => {
                        d.error = Some(e);
                        Task::none()
                    }
                }
            }
            Message::LogsExpand(on) => {
                self.logs_expanded = on;
                Task::none()
            }
            Message::LogsTerminal => {
                let Some(app) = self.logs.as_ref().map(|p| p.app.clone()) else { return Task::none() };
                let (host, dir, tag) = (self.target.host.clone(), self.wardend_dir(), self.tag());
                Task::perform(async move { commands::open_log_terminal(&host, &app, dir.as_deref()).await }, move |r| {
                    tag.wrap(Message::LogsTerminalOpened(r))
                })
            }
            Message::LogsTerminalOpened(Ok(())) => Task::none(),
            Message::LogsTerminalOpened(Err(e)) => self.toast(false, format!("cannot open a terminal: {e}")),
            Message::LogHistory { app, result } => {
                let Some(pane) = self.logs.as_mut().filter(|p| p.app == app) else { return Task::none() };
                match result {
                    Ok(lines) => pane.merge_history(lines),
                    Err(e) => pane.history = logs::History::Failed(e),
                }
                Task::none()
            }
            Message::StartWardend => {
                if self.starting_wardend {
                    return Task::none();
                }
                self.starting_wardend = true;
                let (host, tag) = (self.target.host.clone(), self.tag());
                Task::perform(async move { commands::start_wardend(&host).await }, move |r| {
                    tag.wrap(Message::WardendStarted(r))
                })
            }
            Message::WardendStarted(r) => {
                self.starting_wardend = false;
                match r {
                    Ok(out) if out.ok => {
                        // Connect now, not at the next retry.
                        self.generation += 1;
                        self.toast(true, out.text())
                    }
                    Ok(out) => {
                        self.toast(false, format!("starting wardend failed: {} (`{}`)", out.text(), out.command))
                    }
                    Err(e) => self.toast(false, format!("starting wardend failed: {e}")),
                }
            }
            Message::AskRestartAll => {
                // Asked from an app's "outdated supervisor" banner too: the question is in Settings.
                let opened = if matches!(self.modal, Modal::Settings) {
                    Task::none()
                } else {
                    self.update(Message::OpenSettings)
                };
                if self.restart_all != RestartAll::Idle {
                    return opened;
                }
                self.restart_all = RestartAll::Asking;
                self.restart_who = None;
                // Which warden would run, before anything is stopped.
                let (host, tag) = (self.target.host.clone(), self.tag());
                opened.chain(Task::perform(async move { commands::warden_who(&host).await }, move |r| {
                    tag.wrap(Message::RestartWho(r))
                }))
            }
            Message::RestartWho(r) => {
                if self.restart_all == RestartAll::Asking {
                    self.restart_who = Some(r);
                }
                Task::none()
            }
            Message::CancelRestartAll => {
                if self.restart_all == RestartAll::Asking {
                    self.restart_all = RestartAll::Idle;
                }
                Task::none()
            }
            Message::RestartAll => {
                // Only once the warden that would run is known, and only aimed at the wardend shown.
                let (RestartAll::Asking, Some(Ok(_)), Some(dir)) =
                    (self.restart_all, &self.restart_who, self.wardend_dir())
                else {
                    return Task::none();
                };
                self.restart_all = RestartAll::Running;
                self.restart_secs = 0;
                self.restart_note = None;
                self.resurrecting = false;
                let (host, tag) = (self.target.host.clone(), self.tag());
                Task::perform(async move { commands::restart_everything(&host, Some(&dir)).await }, move |r| {
                    tag.wrap(Message::RestartedAll(r))
                })
            }
            Message::ResurrectAll => {
                if self.restart_all != RestartAll::Idle || self.restart_note.is_none() {
                    return Task::none();
                }
                self.restart_all = RestartAll::Running;
                self.restart_secs = 0;
                self.resurrecting = true;
                let (host, dir, tag) = (self.target.host.clone(), self.wardend_dir(), self.tag());
                Task::perform(async move { commands::resurrect_all(&host, dir.as_deref()).await }, move |r| {
                    tag.wrap(Message::RestartedAll(r))
                })
            }
            Message::RestartTick => {
                if self.restart_all == RestartAll::Running {
                    self.restart_secs += 1;
                }
                Task::none()
            }
            Message::RestartedAll(r) => {
                self.restart_all = RestartAll::Idle;
                self.restart_secs = 0;
                let what = if std::mem::take(&mut self.resurrecting) {
                    "Starting the saved apps"
                } else {
                    "Restart everything"
                };
                // wardend went away and came back: connect now, not at the next retry.
                self.generation += 1;
                self.restart_note = r.stopped.then(|| r.text.clone());
                if r.ok { self.toast(true, r.text) } else { self.toast(false, format!("{what}: {}", r.text)) }
            }
            Message::FromHost { epoch, host, inner } => {
                if epoch == self.epoch {
                    self.update(*inner)
                } else {
                    self.late(&host, *inner)
                }
            }
            Message::InstallCli | Message::InstallCliForMe => {
                let choice = if matches!(message, Message::InstallCliForMe) {
                    cli_install::Choice::ThisUserOnly
                } else {
                    cli_install::Choice::Preferred
                };
                let (Ok(places), cli_install::Work::Idle) = (self.cli.places.clone(), self.cli.work) else {
                    return Task::none();
                };
                self.cli.work = cli_install::Work::Installing;
                Task::perform(cli_install::install(places, choice), Message::CliInstalled)
            }
            Message::CliInstalled(r) => {
                self.cli.work = cli_install::Work::Idle;
                self.cli.refresh();
                self.cli.cancelled = false;
                match (r, &self.cli.places) {
                    (Ok(cli_install::Outcome::Installed(done)), Ok(p)) => self.toast(true, done.summary(&p.cli)),
                    (Ok(cli_install::Outcome::Installed(done)), Err(_)) => {
                        self.toast(true, format!("Installed {}.", done.link.display()))
                    }
                    // A "no" is not an error, and nothing else is done in its place.
                    (Ok(cli_install::Outcome::Cancelled { wanted }), _) => {
                        self.cli.cancelled = true;
                        self.toast(
                            true,
                            format!(
                                "Cancelled: nothing was installed ({} was not made). Settings has \"Install for this \
                                 user only\": a link in your own folder, with no password.",
                                wanted.display()
                            ),
                        )
                    }
                    (Err(e), _) => self.toast(false, format!("installing the command line tool failed: {e}")),
                }
            }
            Message::UninstallCli => {
                let (Ok(places), cli_install::Work::Idle) = (self.cli.places.clone(), self.cli.work) else {
                    return Task::none();
                };
                self.cli.work = cli_install::Work::Removing;
                Task::perform(cli_install::uninstall(places), Message::CliRemoved)
            }
            Message::CliRemoved(r) => {
                self.cli.work = cli_install::Work::Idle;
                self.cli.refresh();
                match r {
                    Ok(gone) => self.toast(true, format!("Removed {}.", gone.link.display())),
                    Err(e) => self.toast(false, format!("removing the command line tool failed: {e}")),
                }
            }
            Message::DismissCliBanner => {
                self.saved.cli_banner_dismissed = true;
                self.persist()
            }
            Message::ToggleMenu(kind) => {
                self.menu = if self.menu == Some(kind) { None } else { Some(kind) };
                Task::none()
            }
            Message::CloseMenu => {
                self.menu = None;
                Task::none()
            }
            Message::UseThisMachine => {
                self.menu = None;
                self.auto_local = true;
                self.switch_target(Target::local(None));
                Task::none()
            }
            Message::UseMachine(dest) => {
                self.menu = None;
                let target = match self.saved.get(&dest) {
                    Some(m) => Target::ssh(&m.dest, &m.remote_socket, &m.remote_warden),
                    None => Err(format!("{dest} is not in the list of machines")),
                };
                match target {
                    Ok(t) => {
                        self.auto_local = false;
                        self.switch_target(t);
                        Task::none()
                    }
                    Err(e) => self.toast(false, format!("cannot connect to {dest}: {e}")),
                }
            }
            Message::AddMachine => {
                self.menu = None;
                self.modal = Modal::Machine(MachineForm {
                    editing: None,
                    dest: String::new(),
                    remote_socket: ssh::default_remote_socket(),
                    remote_warden: "warden".into(),
                    error: None,
                });
                Task::none()
            }
            Message::EditMachine(dest) => {
                self.menu = None;
                if let Some(m) = self.saved.get(&dest) {
                    self.modal = Modal::Machine(MachineForm {
                        editing: Some(m.dest.clone()),
                        dest: m.dest.clone(),
                        remote_socket: m.remote_socket.clone(),
                        remote_warden: m.remote_warden.clone(),
                        error: None,
                    });
                }
                Task::none()
            }
            Message::ForgetMachine(dest) => {
                self.menu = None;
                self.saved.forget(&dest);
                let saved = self.persist();
                Task::batch([saved, self.toast(true, format!("{dest} removed from the list of machines"))])
            }
            Message::MachineDest(s) => self.with_machine_form(|f| f.dest = s),
            Message::MachineSocket(s) => self.with_machine_form(|f| f.remote_socket = s),
            Message::MachineWarden(s) => self.with_machine_form(|f| f.remote_warden = s),
            Message::SaveMachine => {
                let Modal::Machine(f) = &mut self.modal else { return Task::none() };
                let target = Target::ssh(f.dest.trim(), f.remote_socket.trim(), &f.remote_warden);
                match target {
                    Ok(t) => {
                        let Host::Ssh { dest, warden } = &t.host else { return Task::none() };
                        let Endpoint::Ssh(e) = &t.endpoint else { return Task::none() };
                        let machine = Machine {
                            dest: dest.clone(),
                            remote_socket: e.remote_socket.clone(),
                            remote_warden: warden.clone(),
                        };
                        // Editing under another name: the old entry goes.
                        if let Some(old) = f.editing.clone().filter(|o| *o != machine.dest) {
                            self.saved.forget(&old);
                        }
                        self.saved.remember(machine);
                        self.modal = Modal::None;
                        self.auto_local = false;
                        let saved = self.persist();
                        self.switch_target(t);
                        saved
                    }
                    Err(e) => {
                        f.error = Some(e);
                        Task::none()
                    }
                }
            }
            Message::Resized(size) => {
                self.window = size;
                Task::none()
            }
            Message::AppFilter(f) => {
                self.filter = f;
                Task::none()
            }
            Message::Copy(text) => {
                let note = self.toast(true, format!("Copied {text}"));
                Task::batch([iced::clipboard::write(text), note])
            }
            Message::CloseModal => {
                // A running command keeps its dialog until it ends.
                let busy = matches!(&self.modal, Modal::Add(a) if a.status == AddStatus::Running)
                    || matches!(&self.modal, Modal::Editor(e) if e.status == EditorStatus::Saving)
                    || matches!(&self.modal, Modal::Delete(d) if d.running);
                if !busy {
                    self.modal = Modal::None;
                }
                Task::none()
            }
            Message::OpenAdd => {
                self.menu = None;
                self.modal = Modal::Add(AddForm { form: AddApp::default(), status: AddStatus::Editing });
                Task::none()
            }
            Message::AddWhat(s) => self.with_add(|f| f.what = s),
            Message::AddName(s) => self.with_add(|f| f.name = s),
            Message::AddInstances(s) => self.with_add(|f| f.instances = s),
            Message::AddPort(s) => self.with_add(|f| f.port = s),
            Message::AddEnv(s) => self.with_add(|f| f.env_file = s),
            Message::SubmitAdd => {
                let Modal::Add(a) = &mut self.modal else { return Task::none() };
                if a.status == AddStatus::Running {
                    return Task::none();
                }
                if let Err(e) = a.form.args() {
                    a.status = AddStatus::Done(Err(e));
                    return Task::none();
                }
                a.status = AddStatus::Running;
                let (host, form, tag) = (self.target.host.clone(), a.form.clone(), self.tag());
                Task::perform(async move { commands::add_app(&host, &form).await }, move |r| {
                    tag.wrap(Message::Added(r))
                })
            }
            Message::Added(r) => {
                let name = match &self.modal {
                    Modal::Add(a) => a.form.name.trim().to_string(),
                    _ => String::new(),
                };
                let ok = matches!(&r, Ok(a) if a.output.ok);
                if let Modal::Add(a) = &mut self.modal {
                    a.status = AddStatus::Done(r);
                }
                if ok && !name.is_empty() {
                    let t = self.toast(true, format!("{name} added"));
                    return Task::batch([t, self.select(name)]);
                }
                Task::none()
            }
            Message::OpenEditor(app) => {
                let Some(path) = self.model.apps.get(&app).and_then(|a| a.entry.config.clone()) else {
                    return self.toast(false, format!("{app} has no config file that wardend knows of"));
                };
                self.modal = Modal::Editor(Editor {
                    app: app.clone(),
                    path: path.clone(),
                    content: text_editor::Content::new(),
                    status: EditorStatus::Loading,
                    result: None,
                    dirty: false,
                    saved: false,
                });
                let (host, tag) = (self.target.host.clone(), self.tag());
                Task::perform(async move { commands::read_config(&host, &path).await }, move |result| {
                    tag.wrap(Message::EditorLoaded { app: app.clone(), result })
                })
            }
            Message::EditorLoaded { app, result } => {
                let Modal::Editor(e) = &mut self.modal else { return Task::none() };
                if e.app != app {
                    return Task::none();
                }
                match result {
                    Ok(text) => {
                        e.content = text_editor::Content::with_text(&text);
                        e.status = EditorStatus::Ready;
                    }
                    Err(err) => {
                        e.status = EditorStatus::Ready;
                        e.result = Some(Err(err));
                    }
                }
                Task::none()
            }
            Message::Edit(action) => {
                if let Modal::Editor(e) = &mut self.modal {
                    if action.is_edit() {
                        e.dirty = true;
                        e.saved = false;
                    }
                    e.content.perform(action);
                }
                Task::none()
            }
            Message::Validate | Message::Save => {
                let save = matches!(message, Message::Save);
                let Modal::Editor(e) = &mut self.modal else { return Task::none() };
                if e.status != EditorStatus::Ready {
                    return Task::none();
                }
                e.status = if save { EditorStatus::Saving } else { EditorStatus::Checking };
                let (host, path, text) = (self.target.host.clone(), e.path.clone(), e.content.text());
                let tag = self.tag();
                if save {
                    Task::perform(async move { commands::save_config(&host, &path, &text).await }, move |r| {
                        tag.wrap(Message::Saved(r))
                    })
                } else {
                    Task::perform(async move { commands::check_config(&host, &path, &text).await }, move |r| {
                        tag.wrap(Message::Validated(r))
                    })
                }
            }
            Message::Validated(r) | Message::Saved(r) => {
                if let Modal::Editor(e) = &mut self.modal {
                    if r.is_ok() && e.status == EditorStatus::Saving {
                        e.dirty = false;
                        e.saved = true;
                    }
                    e.status = EditorStatus::Ready;
                    e.result = Some(r);
                }
                Task::none()
            }
            Message::ReloadAfterSave => {
                let Modal::Editor(e) = &self.modal else { return Task::none() };
                let app = e.app.clone();
                self.modal = Modal::None;
                self.act(app, Act::Reload)
            }
        }
    }

    fn with_logs(&mut self, f: impl FnOnce(&mut LogPane)) -> Task<Message> {
        if let Some(l) = &mut self.logs {
            f(l);
        }
        Task::none()
    }

    fn with_machine_form(&mut self, f: impl FnOnce(&mut MachineForm)) -> Task<Message> {
        if let Modal::Machine(m) = &mut self.modal {
            f(m);
            m.error = None;
        }
        Task::none()
    }

    /// Write the list of machines; a failure is told, the list stays in memory.
    fn persist(&mut self) -> Task<Message> {
        let Some(path) = self.saved_path.clone() else { return Task::none() };
        match hosts::save(&path, &self.saved) {
            Ok(()) => Task::none(),
            Err(e) => self.toast(false, format!("could not save the settings in {}: {e}", path.display())),
        }
    }

    fn with_add(&mut self, f: impl FnOnce(&mut AddApp)) -> Task<Message> {
        if let Modal::Add(a) = &mut self.modal
            && a.status != AddStatus::Running
        {
            f(&mut a.form);
            if matches!(a.status, AddStatus::Done(Err(_))) {
                a.status = AddStatus::Editing;
            }
        }
        Task::none()
    }

    /// Connect somewhere else: everything known about the old host goes.
    pub fn switch_target(&mut self, t: Target) {
        if t == self.target {
            self.generation += 1;
            return;
        }
        *self = Gui {
            toasts: std::mem::take(&mut self.toasts),
            next_toast: self.next_toast,
            saved: std::mem::take(&mut self.saved),
            saved_path: self.saved_path.take(),
            // This machine's, not the host's.
            cli: self.cli.clone(),
            filter: std::mem::take(&mut self.filter),
            auto_local: self.auto_local,
            window: self.window,
            // Replies of the old host's time (see `epoch`) are then not this host's.
            epoch: self.epoch + 1,
            // The window's look is the person's, not the host's.
            source: self.source,
            system: self.system,
            theme: self.theme.clone(),
            ..Gui::with_target(t)
        };
    }

    fn act(&mut self, app: String, act: Act) -> Task<Message> {
        let socket = match self.request_socket() {
            Ok(s) => s,
            Err(e) => return self.toast(false, format!("{} {app} failed: {e}", act.label())),
        };
        let up = self.model.apps.get(&app).is_some_and(|a| a.supervisor_up());
        self.busy.push((app.clone(), act));
        let done_app = app.clone();
        let tag = self.tag();
        Task::perform(
            async move {
                match act.request(up) {
                    Some(req) => match client::app_request(&socket, &app, req).await {
                        Ok(resp) if resp.ok => Ok(resp.message.unwrap_or_else(|| "done".into())),
                        Ok(resp) => {
                            Err(resp.message.unwrap_or_else(|| "the supervisor refused without saying why".into()))
                        }
                        Err(e) => Err(e),
                    },
                    None => client::start_app(&socket, &app).await,
                }
            },
            move |result| tag.wrap(Message::Done { app: done_app.clone(), act, result }),
        )
    }

    fn on_feed(&mut self, m: FeedMsg) -> Task<Message> {
        match m {
            FeedMsg::Connecting { attempt, what } => {
                // Keep showing why the last attempt failed until this one ends.
                if !matches!(self.conn, Conn::Down { .. }) {
                    self.conn = Conn::Connecting { attempt, what };
                }
                Task::none()
            }
            FeedMsg::Connected { socket } => {
                self.conn = Conn::Connected;
                self.socket = Some(socket);
                // What happened while away (or before this window opened).
                Task::batch([self.fetch_host(), self.fetch_chart()])
            }
            FeedMsg::Batch(b) => {
                let now = now_ms();
                self.model.events_skipped += b.events_dropped;
                self.model.unknown_events += b.unknown;
                for ev in b.events {
                    self.sample(&ev, now / 1000);
                    self.model.apply(ev, now);
                }
                if b.events_dropped > 0 {
                    let line = format!(
                        "{} {} events skipped (this window is slower than the events)",
                        crate::format::clock(now),
                        b.events_dropped
                    );
                    self.model.feed.push(line);
                }
                if self.selected.as_ref().is_some_and(|s| !self.model.apps.contains_key(s)) {
                    self.selected = None;
                    self.logs = None;
                    self.chart = None;
                }
                if self.selected.is_none()
                    && let Some(first) = self.model.sorted().first().map(|a| a.name().to_string())
                {
                    return self.select(first);
                }
                Task::none()
            }
            FeedMsg::Disconnected { error, not_running, retry_in, attempt } => {
                // The logs stream goes with the connection: mark the gap.
                if let (Some(p), true) = (&mut self.logs, std::mem::take(&mut self.logs_live)) {
                    p.note(format!("… disconnected from wardend ({error}); reconnecting"));
                }
                self.conn = Conn::Down { error, not_running, retry_in, attempt };
                self.socket = None;
                self.model.disconnected();
                // wardend may run as another user (a system wardend, `sudo warden startup`)
                // than the socket first guessed: look again, and connect there at once.
                if not_running && self.auto_local {
                    let found = Endpoint::Socket(client::auto_local_socket());
                    if found != self.target.endpoint {
                        self.target.endpoint = found;
                        self.generation += 1;
                    }
                }
                Task::none()
            }
        }
    }

    /// Live samples for the charts: the host's metrics, and the charted
    /// app's statuses and supervisor restarts.
    fn sample(&mut self, ev: &Event, t_s: u64) {
        match ev {
            Event::Host { cpu_percent, mem_used_bytes, .. } => {
                self.host_spark.sample(*cpu_percent, *mem_used_bytes, t_s)
            }
            Event::Status { app, status } => {
                if let Some(c) = self.chart.as_mut().filter(|c| c.app == *app) {
                    c.status(status, t_s);
                }
            }
            Event::Apps { apps } => {
                if let Some(c) = self.chart.as_mut()
                    && let Some(e) = apps.iter().find(|e| e.name == c.app)
                {
                    c.supervisor_restarts(e.supervisor_restarts, t_s);
                }
            }
            _ => {}
        }
    }

    fn on_log_feed(&mut self, m: FeedMsg) -> Task<Message> {
        let tag = self.tag();
        let Some(pane) = &mut self.logs else { return Task::none() };
        match m {
            FeedMsg::Connected { socket } => {
                self.logs_live = true;
                if pane.history == logs::History::Loading {
                    let app = pane.app.clone();
                    return Task::perform(
                        async move {
                            let result = client::app_logs(&socket, &app, logs::HISTORY_LINES).await;
                            (app, result)
                        },
                        move |(app, result)| tag.wrap(Message::LogHistory { app, result }),
                    );
                }
                Task::none()
            }
            FeedMsg::Batch(b) => {
                let lagged: u64 = b
                    .events
                    .iter()
                    .map(|e| if let warden_protocol::events::Event::Lagged { dropped, .. } = e { *dropped } else { 0 })
                    .sum();
                let lines: Vec<String> = b.logs.into_iter().filter(|(a, _)| *a == pane.app).map(|(_, l)| l).collect();
                if lines.is_empty() && lagged + b.logs_dropped == 0 {
                    return Task::none();
                }
                pane.push(lines, lagged + b.logs_dropped);
                Task::none()
            }
            FeedMsg::Disconnected { error, .. } => {
                if std::mem::take(&mut self.logs_live) {
                    pane.note(format!("… the log stream stopped ({error}); reconnecting"));
                }
                Task::none()
            }
            FeedMsg::Connecting { .. } => Task::none(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let main = Subscription::run_with(
            MainKey { endpoint: self.target.endpoint.clone(), generation: self.generation },
            main_feed,
        )
        .map(Message::Feed);
        let logs = match (&self.logs, &self.socket, &self.conn) {
            (Some(p), Some(socket), Conn::Connected) => Subscription::run_with(
                LogKey { socket: socket.clone(), app: p.app.clone(), generation: self.generation },
                log_feed,
            )
            .map(Message::LogFeed),
            _ => Subscription::none(),
        };
        let size = iced::window::resize_events().map(|(_, size)| Message::Resized(size));
        let mut all = vec![main, logs, size];
        if self.restart_all == RestartAll::Running {
            all.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::RestartTick));
        }
        if self.source.follows_system() {
            // The desktop's look: GNOME says when it changes; elsewhere it is read
            // again when the window is focused or the mode changes.
            all.push(Subscription::run(system::watch).map(Message::System));
            all.push(iced::system::theme_changes().map(|_| Message::ProbeSystem));
            all.push(iced::event::listen_with(|event, _, _| match event {
                iced::Event::Window(iced::window::Event::Focused) => Some(Message::ProbeSystem),
                _ => None,
            }));
        }
        Subscription::batch(all)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MainKey {
    endpoint: Endpoint,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LogKey {
    socket: PathBuf,
    app: String,
    generation: u64,
}

fn main_feed(k: &MainKey) -> impl iced::futures::Stream<Item = FeedMsg> + Send + use<> {
    client::feed(k.endpoint.clone(), FeedOptions::all())
}

fn log_feed(k: &LogKey) -> impl iced::futures::Stream<Item = FeedMsg> + Send + use<> {
    client::feed(Endpoint::Socket(k.socket.clone()), FeedOptions::logs_of(&k.app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Batch;
    use crate::model::tests::{entry, status};
    use warden_protocol::events::{AppState, Event};

    fn gui() -> Gui {
        Gui::with_target(Target::local(Some("/tmp/test-wardend.sock".into())))
    }

    fn batch(events: Vec<Event>) -> Message {
        Message::Feed(FeedMsg::Batch(Batch { events, ..Batch::default() }))
    }

    fn connected() -> Gui {
        let mut g = gui();
        let _ = g.update(Message::Feed(FeedMsg::Connected { socket: "/tmp/test-wardend.sock".into() }));
        let _ = g.update(batch(vec![
            Event::Hello { protocol: 1, app: None, pid: 9, version: "0.1.0".into() },
            Event::Apps {
                apps: vec![
                    entry("api", AppState::Running, Some(status("api", 2))),
                    entry("web", AppState::NotStarted, None),
                ],
            },
        ]));
        g
    }

    #[test]
    fn feed_messages_drive_the_connection_state() {
        let mut g = gui();
        assert!(matches!(g.conn, Conn::Connecting { .. }) && !g.connected());
        let _ = g.update(Message::Feed(FeedMsg::Disconnected {
            error: "wardend is not running".into(),
            not_running: true,
            retry_in: Duration::from_millis(250),
            attempt: 1,
        }));
        assert!(matches!(g.conn, Conn::Down { not_running: true, .. }));
        // A new attempt keeps the reason of the last failure on screen.
        let _ = g.update(Message::Feed(FeedMsg::Connecting { attempt: 2, what: "x".into() }));
        assert!(matches!(g.conn, Conn::Down { .. }));
        let mut g = connected();
        assert!(g.connected());
        assert_eq!(g.selected.as_deref(), Some("api"), "the first app is selected");
        assert_eq!(g.model.daemon.as_ref().map(|d| d.pid), Some(9));
        let _ = g.update(Message::Feed(FeedMsg::Disconnected {
            error: "gone".into(),
            not_running: false,
            retry_in: Duration::from_secs(1),
            attempt: 1,
        }));
        assert!(!g.connected() && g.socket.is_none() && g.model.daemon.is_none());
        assert!(g.model.apps.contains_key("api"), "apps stay (as last seen) while reconnecting");
    }

    #[test]
    fn selection_follows_the_app_set() {
        let mut g = connected();
        let _ = g.update(Message::Select("web".into()));
        assert_eq!(g.selected.as_deref(), Some("web"));
        let _ = g.update(batch(vec![Event::Apps { apps: vec![entry("api", AppState::Running, None)] }]));
        assert_eq!(g.selected.as_deref(), Some("api"), "web is gone: the first app is selected");
    }

    #[test]
    fn destructive_actions_ask_first() {
        let mut g = connected();
        let _ = g.update(Message::Act("api".into(), Act::Stop));
        assert_eq!(g.confirm, Some(Pending { app: "api".into(), act: Act::Stop }));
        assert!(g.busy.is_empty(), "nothing sent before the answer");
        let _ = g.update(Message::Cancelled);
        assert!(g.confirm.is_none() && g.busy.is_empty());
        let _ = g.update(Message::Act("api".into(), Act::Scale(0)));
        let _ = g.update(Message::Confirmed);
        assert!(g.confirm.is_none());
        assert_eq!(g.busy, [("api".to_string(), Act::Scale(0))]);
        let _ = g.update(Message::Act("api".into(), Act::Reload));
        assert!(g.confirm.is_none(), "a reload has no downtime: no question");
        assert_eq!(g.busy.len(), 2);
        assert!(Act::HardRestart.destructive() && Act::Scale(0).destructive() && !Act::Scale(1).destructive());
        assert!(Act::Stop.question("api").contains("Start brings"));
    }

    #[test]
    fn requests_for_each_action() {
        assert_eq!(Act::SafeReload.request(true), Some(Request::Reload { safe: true }));
        assert_eq!(Act::HardRestart.request(true), Some(Request::Restart { worker: None, hard: true }));
        assert_eq!(Act::RestartWorker(3).request(true), Some(Request::Restart { worker: Some(3), hard: false }));
        assert_eq!(Act::Scale(4).request(true), Some(Request::Scale { count: 4 }));
        assert_eq!(Act::Start.request(true), Some(Request::Start));
        assert_eq!(Act::Start.request(false), None, "no supervisor: wardend starts it");
        assert_eq!(Act::Reset.request(true), Some(Request::Reset { worker: None }));
    }

    #[test]
    fn results_become_toasts_and_errors_say_what_failed() {
        let mut g = connected();
        let _ = g.update(Message::Act("api".into(), Act::Reload));
        let _ = g.update(Message::Done {
            app: "api".into(),
            act: Act::Reload,
            result: Ok("reload started (seq 3)".into()),
        });
        assert!(g.busy.is_empty());
        assert_eq!(g.toasts.last().map(|t| (t.ok, t.text.as_str())), Some((true, "api: reload started (seq 3)")));
        let _ = g.update(Message::Done {
            app: "api".into(),
            act: Act::Stop,
            result: Err("no such worker; see `warden list`".into()),
        });
        let t = g.toasts.last().unwrap();
        assert!(!t.ok && t.text == "Stop api failed: no such worker; see `warden list`", "{t:?}");
        for i in 0..10 {
            let _ = g.update(Message::Done { app: "api".into(), act: Act::Reload, result: Ok(format!("{i}")) });
        }
        assert_eq!(g.toasts.len(), TOASTS, "bounded");
        let id = g.toasts[0].id;
        let _ = g.update(Message::DismissToast(id));
        assert!(g.toasts.iter().all(|t| t.id != id));
        // Not connected: the error says so, nothing is sent.
        let mut d = gui();
        let _ = d.update(Message::Act("api".into(), Act::Reload));
        assert!(d.busy.is_empty() && d.toasts[0].text.contains("not connected"), "{:?}", d.toasts);
    }

    #[test]
    fn logs_tab_opens_a_pane_that_takes_batches() {
        let mut g = connected();
        assert!(g.logs.is_none());
        let _ = g.update(Message::Tab(Tab::Logs));
        assert_eq!(g.logs.as_ref().map(|l| l.app.as_str()), Some("api"));
        let _ = g.update(Message::LogFeed(FeedMsg::Connected { socket: "/tmp/test-wardend.sock".into() }));
        assert!(g.logs_live);
        let mut b = Batch::default();
        b.logs.push_back(("api".into(), "2026-09-30T12:00:00.000Z OUT   worker=1 stdout: hi".into()));
        b.logs.push_back(("web".into(), "not ours".into()));
        b.logs_dropped = 7;
        let _ = g.update(Message::LogFeed(FeedMsg::Batch(b)));
        let pane = g.logs.as_ref().unwrap();
        let texts: Vec<&str> = pane.visible().iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[0].starts_with("… 7 lines skipped") && texts[1].ends_with("stdout: hi"));
        let _ = g.update(Message::LogHistory { app: "api".into(), result: Ok(vec!["old".into()]) });
        assert_eq!(g.logs.as_ref().unwrap().visible()[0].text, "old");
        let _ = g.update(Message::LogsFilter("hi".into()));
        let _ = g.update(Message::LogsPause(true));
        assert!(g.logs.as_ref().unwrap().paused);
        let _ = g.update(Message::LogFeed(FeedMsg::Disconnected {
            error: "gone".into(),
            not_running: false,
            retry_in: Duration::from_secs(1),
            attempt: 1,
        }));
        assert!(!g.logs_live);
        let _ = g.update(Message::Select("web".into()));
        assert_eq!(g.logs.as_ref().map(|l| l.app.as_str()), Some("web"), "a new pane for the new app");
        let _ = g.update(Message::Tab(Tab::Events));
        assert!(g.logs.is_none());
    }

    fn history(app: &str, step: u32, points: u32, start_s: u64) -> ResourceHistory {
        let n = points as usize;
        ResourceHistory {
            start_ms: start_s * 1000,
            step_s: step,
            points,
            host: warden_protocol::events::HostHistory {
                cpu_percent: vec![Some(5.0); n],
                mem_used_bytes: vec![Some(1 << 30); n],
                mem_total_bytes: Some(4 << 30),
                load1: vec![None; n],
            },
            apps: vec![warden_protocol::events::AppHistory {
                app: app.into(),
                cpu_percent: vec![Some(1.0); n],
                rss_bytes: vec![Some(1 << 20); n],
                workers_ready: vec![Some(2); n],
                workers_configured: vec![Some(2); n],
                restarts: vec![Some(0); n],
            }],
        }
    }

    #[test]
    fn the_history_tab_fetches_follows_the_selection_and_adds_live_samples() {
        let mut g = connected();
        assert!(g.chart.is_none(), "no charts until the History tab shows");
        let _ = g.update(Message::Tab(Tab::History));
        let c = g.chart.as_ref().unwrap();
        assert_eq!((c.app.as_str(), c.range, &c.load), ("api", Range::Hour, &Load::Loading));
        // A reply for another app or range (an older request) is ignored.
        let now = now_ms() / 1000;
        let start = now - now % 10 - 50;
        let _ = g.update(Message::HistoryLoaded {
            app: "api".into(),
            range: Range::Day,
            result: Ok(history("api", 240, 3, start)),
        });
        assert_eq!(g.chart.as_ref().unwrap().load, Load::Loading);
        let _ = g.update(Message::HistoryLoaded {
            app: "api".into(),
            range: Range::Hour,
            result: Ok(history("api", 10, 5, start)),
        });
        let c = g.chart.as_ref().unwrap();
        assert_eq!((c.load.clone(), c.grid.len()), (Load::Ready, 5));
        // A live status of the app lands on the grid; another app's does not.
        let _ = g.update(batch(vec![Event::Status { app: "web".into(), status: Box::new(status("web", 1)) }]));
        let _ = g.update(batch(vec![Event::Status { app: "api".into(), status: Box::new(status("api", 2)) }]));
        let c = g.chart.as_ref().unwrap();
        assert!(c.grid.len() >= 5 && c.grid.series[crate::history::CPU].last() == Some(3.0), "{:?}", c.grid);
        // Another range: the charts stay, faded, while it loads.
        let _ = g.update(Message::HistoryRange(Range::SixHours));
        let c = g.chart.as_ref().unwrap();
        assert_eq!((c.range, c.load.clone(), c.grid.is_empty()), (Range::SixHours, Load::Loading, false));
        let _ = g.update(Message::HistoryLoaded {
            app: "api".into(),
            range: Range::SixHours,
            result: Err("this wardend keeps no history".into()),
        });
        assert!(matches!(&g.chart.as_ref().unwrap().load, Load::Failed(e) if e.contains("no history")));
        // Another app: its own charts, in the range chosen.
        let _ = g.update(Message::Select("web".into()));
        let c = g.chart.as_ref().unwrap();
        assert_eq!((c.app.as_str(), c.range, c.grid.is_empty()), ("web", Range::SixHours, true));
        let _ = g.update(Message::Tab(Tab::Events));
        assert!(g.chart.is_none());
        // The header's sparklines: the reply, then host events.
        let _ = g.update(Message::HostHistoryLoaded(Ok(history("", 10, 4, start))));
        assert_eq!(g.host_spark.grid.len(), 4);
        let _ = g.update(batch(vec![Event::Host {
            cpu_percent: 50.0,
            mem_used_bytes: 2 << 30,
            mem_total_bytes: 4 << 30,
            load: [0.0; 3],
            at_ms: 0,
        }]));
        assert_eq!(g.host_spark.grid.series[crate::history::HOST_CPU].last(), Some(50.0));
    }

    fn who() -> commands::Who {
        commands::Who { command: "/Applications/Warden.app/Contents/MacOS/warden".into(), version: "0.1.0".into() }
    }

    /// Connected to a wardend that `warden` can be pointed at (its socket is `wardend.sock`).
    fn connected_to_a_runtime_dir() -> Gui {
        let mut g = connected();
        g.target.endpoint = Endpoint::Socket("/tmp/wg-test-run/wardend.sock".into());
        g
    }

    #[test]
    fn delete_runs_only_once_the_apps_name_is_typed() {
        let mut g = connected_to_a_runtime_dir();
        let _ = g.update(Message::AskDelete("api".into()));
        let form = |g: &Gui| match &g.modal {
            Modal::Delete(d) => d.clone(),
            _ => panic!("no delete dialog"),
        };
        let _ = g.update(Message::ConfirmDelete);
        assert!(!form(&g).running, "nothing typed: nothing runs");
        let _ = g.update(Message::DeleteTyped("ap".into()));
        let _ = g.update(Message::ConfirmDelete);
        assert!(!form(&g).running, "a part of the name is not enough");
        let _ = g.update(Message::DeleteTyped("API".into()));
        let _ = g.update(Message::ConfirmDelete);
        assert!(!form(&g).running, "exactly the name");
        let _ = g.update(Message::DeleteTyped("api".into()));
        let _ = g.update(Message::ConfirmDelete);
        assert!(form(&g).running);
        let _ = g.update(Message::CloseModal);
        assert!(matches!(g.modal, Modal::Delete(_)), "kept while it runs");
        let _ = g.update(Message::Deleted(Err("no app named api".into())));
        assert_eq!(form(&g).error.as_deref(), Some("no app named api"));
        let ok = commands::Output {
            command: "warden delete api".into(),
            ok: true,
            code: Some(0),
            stdout: "api: stopped".into(),
            stderr: String::new(),
        };
        let _ = g.update(Message::ConfirmDelete);
        let _ = g.update(Message::Deleted(Ok(ok)));
        assert!(matches!(g.modal, Modal::None));
        assert!(g.toasts.iter().any(|t| t.text.contains("api deleted")));
    }

    #[test]
    fn restarting_everything_asks_first_names_the_warden_and_reconnects_after() {
        let mut g = connected_to_a_runtime_dir();
        assert_eq!(g.restart_all, RestartAll::Idle);
        // Nothing runs without the second click.
        let _ = g.update(Message::RestartAll);
        assert_eq!(g.restart_all, RestartAll::Idle, "a confirm without a question does nothing");
        let _ = g.update(Message::AskRestartAll);
        assert_eq!(g.restart_all, RestartAll::Asking);
        assert!(
            matches!(g.modal, Modal::Settings),
            "asked from an app's banner, it opens Settings, where the question is"
        );
        assert!(g.restart_who.is_none(), "it is being looked up");
        let _ = g.update(Message::RestartAll);
        assert_eq!(g.restart_all, RestartAll::Asking, "not before it is known which warden would run");
        let _ = g.update(Message::CancelRestartAll);
        assert_eq!(g.restart_all, RestartAll::Idle);
        let _ = g.update(Message::AskRestartAll);
        let _ = g.update(Message::RestartWho(Err("the warden CLI was not found".into())));
        let _ = g.update(Message::RestartAll);
        assert_eq!(g.restart_all, RestartAll::Asking, "and not when there is none");
        let _ = g.update(Message::CancelRestartAll);
        let _ = g.update(Message::AskRestartAll);
        let _ = g.update(Message::RestartWho(Ok(who())));
        let _ = g.update(Message::RestartAll);
        assert_eq!(g.restart_all, RestartAll::Running);
        // Asking again while it runs changes nothing; a second ticks.
        let _ = g.update(Message::AskRestartAll);
        assert_eq!(g.restart_all, RestartAll::Running);
        let _ = g.update(Message::RestartTick);
        let _ = g.update(Message::RestartTick);
        assert_eq!(g.restart_secs, 2, "the dialog shows how long it has been");
        // Done: connect to the new wardend now, and say so.
        let before = g.generation;
        let ok = commands::Restart { ok: true, text: "Restarted every supervisor and wardend.".into(), stopped: false };
        let _ = g.update(Message::RestartedAll(ok));
        assert_eq!(g.restart_all, RestartAll::Idle);
        assert_eq!(g.restart_secs, 0);
        assert_eq!(g.generation, before + 1);
        assert!(g.restart_note.is_none());
        assert!(g.toasts.last().is_some_and(|t| t.ok && t.text.contains("Restarted")));
    }

    #[test]
    fn a_restart_for_a_socket_warden_cannot_be_pointed_at_is_not_offered() {
        // `--socket /tmp/custom.sock`: the CLI finds wardend as wardend.sock in its runtime directory only.
        let mut g = connected();
        g.target.endpoint = Endpoint::Socket("/tmp/custom.sock".into());
        assert_eq!(g.wardend_dir(), None);
        let _ = g.update(Message::AskRestartAll);
        let _ = g.update(Message::RestartWho(Ok(who())));
        let _ = g.update(Message::RestartAll);
        assert_eq!(g.restart_all, RestartAll::Asking, "it would restart another wardend");
        // A remote one is aimed by its socket's directory.
        g.switch_target(Target::ssh("deploy@web-1", "/run/warden/wardend.sock", "warden").unwrap());
        assert_eq!(g.wardend_dir(), Some(PathBuf::from("/run/warden")));
    }

    #[test]
    fn a_restart_that_did_not_finish_says_the_apps_may_be_stopped_and_offers_the_way_back() {
        let mut g = connected_to_a_runtime_dir();
        let _ = g.update(Message::AskRestartAll);
        let _ = g.update(Message::RestartWho(Ok(who())));
        let _ = g.update(Message::RestartAll);
        let lost = commands::Restart {
            ok: false,
            text: "it failed: `warden update --yes` did not finish within 900 s; it was stopped. Apps may be stopped: \
                   what was running was saved first, so starting it again brings them back (use \"Start the saved apps again\" in Settings, or run `warden resurrect`)."
                .into(),
            stopped: true,
        };
        let _ = g.update(Message::RestartedAll(lost));
        let t = g.toasts.last().unwrap();
        assert!(!t.ok && t.text.contains("Apps may be stopped"), "{}", t.text);
        assert!(g.restart_note.as_deref().is_some_and(|n| n.contains("resurrect")), "it stays in Settings");
        // The way back is one click, and it is only there after such a failure.
        let _ = g.update(Message::ResurrectAll);
        assert_eq!(g.restart_all, RestartAll::Running);
        assert!(g.resurrecting);
        let back = commands::Restart { ok: true, text: "Started the saved apps again.".into(), stopped: false };
        let _ = g.update(Message::RestartedAll(back));
        assert!(g.restart_note.is_none(), "the note goes when the apps are back");
        assert!(!g.resurrecting);
        let _ = g.update(Message::ResurrectAll);
        assert_eq!(g.restart_all, RestartAll::Idle, "nothing to bring back: nothing runs");
        // A failure before anything was stopped says so and leaves no alarm.
        let _ = g.update(Message::AskRestartAll);
        let _ = g.update(Message::RestartWho(Ok(who())));
        let _ = g.update(Message::RestartAll);
        let untouched = commands::Restart {
            ok: false,
            text: "it failed: the save failed, so nothing was stopped. Nothing was stopped.".into(),
            stopped: false,
        };
        let _ = g.update(Message::RestartedAll(untouched));
        assert!(g.restart_note.is_none());
        assert!(g.toasts.last().unwrap().text.contains("Nothing was stopped"));
    }

    #[test]
    fn a_reply_from_before_a_host_switch_changes_nothing() {
        let mut g = connected_to_a_runtime_dir();
        let old = g.tag();
        assert_eq!((g.epoch, old.epoch), (0, 0));
        // An action and an open editor on the first host...
        g.busy.push(("api".into(), Act::Reload));
        g.modal = Modal::Editor(Editor {
            app: "api".into(),
            path: "/etc/warden/api.toml".into(),
            content: text_editor::Content::new(),
            status: EditorStatus::Loading,
            result: None,
            dirty: false,
            saved: false,
        });
        // ...then the person switches host, and starts the same on the new one.
        g.switch_target(Target::ssh("deploy@web-1", "/run/warden/wardend.sock", "warden").unwrap());
        assert_eq!(g.epoch, 1, "a switch is counted, and never counted back");
        g.busy.push(("api".into(), Act::Reload));
        g.modal = Modal::Editor(Editor {
            app: "api".into(),
            path: "/etc/warden/api.toml".into(),
            content: text_editor::Content::new(),
            status: EditorStatus::Loading,
            result: None,
            dirty: false,
            saved: false,
        });
        let (toasts, generation) = (g.toasts.len(), g.generation);
        // The first host's answers arrive now.
        let done = Message::Done { app: "api".into(), act: Act::Reload, result: Ok("reloaded".into()) };
        let _ = g.update(old.wrap(done));
        assert_eq!(g.busy.len(), 1, "the new host's busy flag stays");
        let text = Message::EditorLoaded { app: "api".into(), result: Ok("the first host's config".into()) };
        let _ = g.update(old.wrap(text));
        assert!(matches!(&g.modal, Modal::Editor(e) if e.status == EditorStatus::Loading), "not its text");
        let restarted = commands::Restart { ok: true, text: "Restarted every supervisor.".into(), stopped: false };
        let _ = g.update(old.wrap(Message::RestartedAll(restarted)));
        assert_eq!(g.generation, generation, "a late restart does not tear down this host's connection");
        let _ = g.update(old.wrap(Message::WardendStarted(Ok(Output {
            command: "warden wardend --background".into(),
            ok: true,
            code: Some(0),
            stdout: "started".into(),
            stderr: String::new(),
        }))));
        assert_eq!(g.generation, generation);
        assert_eq!(g.toasts.len(), toasts, "no toast about a success on another host");
        // A failure there is still told, with the host it was on.
        let failed = Message::Done { app: "api".into(), act: Act::Reload, result: Err("refused".into()) };
        let _ = g.update(old.wrap(failed));
        let t = g.toasts.last().unwrap();
        assert!(!t.ok && t.text.starts_with("This machine:") && t.text.contains("refused"), "{}", t.text);
        assert_eq!(g.busy.len(), 1);
        // The new host's own reply is applied.
        let done = Message::Done { app: "api".into(), act: Act::Reload, result: Ok("reloaded".into()) };
        let _ = g.update(g.tag().wrap(done));
        assert!(g.busy.is_empty());
        // Reconnecting to the same host is not a switch.
        let same = g.target.clone();
        g.switch_target(same);
        assert_eq!(g.epoch, 1);
    }

    /// A fake `Warden.app` and the folders of a Linux home, all under one temporary folder.
    fn cli_places(name: &str) -> (PathBuf, cli_install::Places) {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("wg-app-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let macos = root.join("Warden.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        for f in ["warden-gui", "warden"] {
            std::fs::write(macos.join(f), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(macos.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let home = root.join("home");
        let places = cli_install::Places {
            exe: macos.join("warden-gui"),
            cli: macos.join("warden"),
            system_dir: None,
            user_dir: Some(home.join(".local/bin")),
            search: vec![home.join(".local/bin")],
            path: vec![],
            shell: Some("/bin/zsh".into()),
            home: Some(home),
            mac: false,
        };
        (root, places)
    }

    #[test]
    fn a_damaged_settings_file_is_told_and_kept_when_the_window_starts() {
        let dir = std::env::temp_dir().join(format!("wg-app-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gui.json");
        let text = r#"{"machines":[{"dest":"deploy@web-1""#;
        std::fs::write(&file, text).unwrap();
        let mut g = connected();
        g.saved_path = Some(file.clone());
        let _ = g.read_saved();
        let t = g.toasts.last().expect("a toast says so");
        assert!(!t.ok && t.text.contains("gui.json.bad"), "{}", t.text);
        // Changing a setting writes a new file; the damaged one is still there to be mended by hand.
        let _ = g.update(Message::DismissCliBanner);
        assert!(hosts::load(&file).cli_banner_dismissed);
        assert_eq!(std::fs::read_to_string(hosts::bad_path(&file)).unwrap(), text);
        // A file that cannot be read is not written over, and the window says the settings are not kept.
        let unreadable = dir.join("dir.json");
        std::fs::create_dir(&unreadable).unwrap();
        let mut h = connected();
        h.saved_path = Some(unreadable.clone());
        let _ = h.read_saved();
        assert!(h.saved_path.is_none(), "nothing is written there");
        assert!(h.toasts.last().unwrap().text.contains("not saved"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_command_line_tool_is_installed_and_removed_from_the_window() {
        use cli_install::{State, Status, Work};
        let (root, places) = cli_places("flow");
        let file = root.join("config/gui.json");
        let mut g = connected();
        g.saved_path = Some(file.clone());
        // A window that looked at nothing (or cannot link from here) does nothing.
        let _ = g.update(Message::InstallCli);
        let _ = g.update(Message::UninstallCli);
        assert_eq!(g.cli.work, Work::Idle);
        g.cli = State::of(Ok(places.clone()));
        assert_eq!(g.cli.status, Status::Missing);
        assert!(g.cli.banner(g.saved.cli_banner_dismissed), "a bundle, no warden: the banner offers it");
        // One click starts it; a second while it runs is ignored.
        let _ = g.update(Message::InstallCli);
        assert_eq!(g.cli.work, Work::Installing);
        let _ = g.update(Message::UninstallCli);
        assert_eq!(g.cli.work, Work::Installing, "one action at a time");
        // What the task does, on the temporary folders (no administrator: there is no system folder).
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let done = rt.block_on(cli_install::install(places.clone(), cli_install::Choice::Preferred));
        let _ = g.update(Message::CliInstalled(done));
        assert_eq!(g.cli.work, Work::Idle);
        let link = places.user_dir.clone().unwrap().join("warden");
        assert!(matches!(&g.cli.status, Status::Linked { link: l, .. } if *l == link), "{:?}", g.cli.status);
        let t = g.toasts.last().unwrap();
        assert!(t.ok && t.text.contains("Installed") && t.text.contains(link.to_str().unwrap()), "{}", t.text);
        assert!(!g.cli.banner(false), "installed: no banner");
        // The window follows the host it shows, but the tool is this machine's.
        g.switch_target(Target::ssh("deploy@web-1", "/run/warden/wardend.sock", "warden").unwrap());
        assert!(matches!(g.cli.status, Status::Linked { .. }));
        // Remove it again.
        let _ = g.update(Message::UninstallCli);
        assert_eq!(g.cli.work, Work::Removing);
        let gone = rt.block_on(cli_install::uninstall(places.clone()));
        let _ = g.update(Message::CliRemoved(gone));
        assert_eq!((g.cli.work, &g.cli.status), (Work::Idle, &Status::Missing));
        assert!(g.toasts.last().is_some_and(|t| t.ok && t.text.starts_with("Removed")));
        // Failures are told with what failed.
        let _ = g.update(Message::CliInstalled(Err("/usr/local/bin/warden is a file, not a link".into())));
        let t = g.toasts.last().unwrap();
        assert!(!t.ok && t.text.contains("installing the command line tool failed") && t.text.contains("is a file"));
        let _ = g.update(Message::CliRemoved(Err("nothing".into())));
        assert!(g.toasts.last().is_some_and(|t| !t.ok && t.text.contains("removing the command line tool failed")));
        // "Not now" is kept in gui.json, and a new window starts with it.
        assert!(!g.saved.cli_banner_dismissed);
        let _ = g.update(Message::DismissCliBanner);
        assert!(g.saved.cli_banner_dismissed && !g.cli.banner(g.saved.cli_banner_dismissed));
        assert!(hosts::load(&file).cli_banner_dismissed);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancelled_administrator_prompt_installs_nothing_and_offers_the_users_folder() {
        use cli_install::{Outcome, State, Status, Work};
        let (root, places) = cli_places("cancel");
        let mut g = connected();
        g.cli = State::of(Ok(places.clone()));
        let _ = g.update(Message::InstallCli);
        assert_eq!(g.cli.work, Work::Installing);
        // The prompt was closed.
        let wanted = std::path::PathBuf::from("/usr/local/bin/warden");
        let _ = g.update(Message::CliInstalled(Ok(Outcome::Cancelled { wanted })));
        assert_eq!((g.cli.work, &g.cli.status), (Work::Idle, &Status::Missing), "nothing was installed");
        assert!(g.cli.cancelled, "Settings says so");
        let t = g.toasts.last().unwrap();
        assert!(t.text.contains("Cancelled") && t.text.contains("nothing was installed"), "{}", t.text);
        assert!(t.text.contains("Install for this user only"), "{}", t.text);
        assert!(!places.user_dir.clone().unwrap().join("warden").exists(), "not even in the users folder");
        // The users folder is its own button, and works with no prompt.
        let _ = g.update(Message::InstallCliForMe);
        assert_eq!(g.cli.work, Work::Installing);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let done = rt.block_on(cli_install::install(places.clone(), cli_install::Choice::ThisUserOnly));
        let _ = g.update(Message::CliInstalled(done));
        assert!(!g.cli.cancelled);
        assert!(matches!(g.cli.status, Status::Linked { .. }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_choose_the_colors_and_keep_them() {
        use crate::system::{Accent, Colors, Flavor, Mode, Source, System};
        let dir = std::env::temp_dir().join(format!("wg-app-settings-{}", std::process::id()));
        let file = dir.join("gui.json");
        let mut g = connected();
        g.saved_path = Some(file.clone());
        // Warden's green, as the desktop's mode says (dark when it says nothing).
        assert_eq!((g.source, g.theme.to_string().as_str()), (Source::default(), "Warden dark"));
        let _ = g.update(Message::OpenSettings);
        assert!(matches!(g.modal, Modal::Settings));
        let mac = System { flavor: Flavor::Mac, dark: Some(false), accent: Accent::Purple };
        let _ = g.update(Message::System(mac));
        assert_eq!(g.theme.to_string(), "Warden light", "Auto follows the desktop's mode");
        // The desktop's colors.
        let _ = g.update(Message::SetColors(Colors::System));
        assert_eq!(g.theme.to_string(), "Mac light Purple");
        let _ = g.update(Message::System(System { dark: Some(true), ..mac }));
        assert_eq!(g.theme.to_string(), "Mac dark Purple");
        // A pinned mode ignores the desktop's, the colors stay.
        let _ = g.update(Message::SetMode(Mode::Light));
        assert_eq!(g.theme.to_string(), "Mac light Purple");
        let _ = g.update(Message::System(System { dark: Some(true), accent: Accent::Teal, ..mac }));
        assert_eq!(g.theme.to_string(), "Mac light Teal", "the accent still follows");
        // It is kept in gui.json, next to the machines, and a new window starts with it.
        let kept = hosts::load(&file).appearance;
        assert_eq!(kept, Source { colors: Colors::System, mode: Mode::Light });
        assert_eq!(g.saved.appearance, kept);
        // Back to Warden: no longer follows the desktop; its changes change nothing.
        let _ = g.update(Message::SetColors(Colors::Warden));
        let _ = g.update(Message::SetMode(Mode::Dark));
        assert_eq!(g.theme.to_string(), "Warden dark");
        let _ = g.update(Message::System(mac));
        assert_eq!(g.theme.to_string(), "Warden dark");
        // Choosing another machine keeps the look.
        g.switch_target(Target::ssh("deploy@web-1", "/run/warden/wardend.sock", "warden").unwrap());
        assert_eq!(g.theme.to_string(), "Warden dark");
        assert_eq!(g.source, Source { colors: Colors::Warden, mode: Mode::Dark });
        // Another desktop than macOS and GNOME: Warden's colors, whatever was asked.
        let _ = g.update(Message::SetColors(Colors::System));
        let _ = g.update(Message::SetMode(Mode::Auto));
        let _ = g.update(Message::System(System { flavor: Flavor::Other, dark: Some(false), accent: Accent::Blue }));
        assert_eq!(g.theme.to_string(), "Warden light");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machine_form_validates_saves_and_connects() {
        let dir = std::env::temp_dir().join(format!("wg-app-machines-{}", std::process::id()));
        let file = dir.join("gui.json");
        let mut g = connected();
        g.saved_path = Some(file.clone());
        let _ = g.update(Message::AddMachine);
        let _ = g.update(Message::MachineDest("-oProxyCommand=x".into()));
        let _ = g.update(Message::SaveMachine);
        match &g.modal {
            Modal::Machine(f) => assert!(f.error.as_deref().is_some_and(|e| e.contains("option"))),
            _ => panic!("the form stays open with the error"),
        }
        assert!(g.saved.machines.is_empty(), "a refused machine is not kept");
        let _ = g.update(Message::MachineDest("deploy@web-1".into()));
        let _ = g.update(Message::SaveMachine);
        assert!(matches!(g.modal, Modal::None));
        assert_eq!(g.target.host, Host::Ssh { dest: "deploy@web-1".into(), warden: "warden".into() });
        assert!(g.model.apps.is_empty() && g.selected.is_none(), "nothing of the old host remains");
        assert_eq!(g.title(), "Warden: deploy@web-1");
        assert_eq!(g.saved.machines.len(), 1, "saved, and the list survives the switch");
        assert_eq!(hosts::load(&file), g.saved, "and written to disk");
        let before = g.generation;
        g.switch_target(g.target.clone());
        assert_eq!(g.generation, before + 1, "the same target again: reconnect now");
        // Back to this machine, and to the saved one from the menu.
        let _ = g.update(Message::UseThisMachine);
        assert_eq!(g.title(), "Warden");
        assert_eq!(g.saved.machines.len(), 1, "going home keeps the list");
        let _ = g.update(Message::UseMachine("deploy@web-1".into()));
        assert_eq!(g.title(), "Warden: deploy@web-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn machines_are_edited_renamed_and_forgotten() {
        let dir = std::env::temp_dir().join(format!("wg-app-edit-{}", std::process::id()));
        let file = dir.join("gui.json");
        let mut g = connected();
        g.saved_path = Some(file.clone());
        for dest in ["a@one", "b@two"] {
            let _ = g.update(Message::AddMachine);
            let _ = g.update(Message::MachineDest(dest.into()));
            let _ = g.update(Message::SaveMachine);
        }
        assert_eq!(g.saved.machines.len(), 2);
        // Editing opens the form filled in; a new name replaces the old entry.
        let _ = g.update(Message::EditMachine("a@one".into()));
        match &g.modal {
            Modal::Machine(f) => assert_eq!((f.editing.as_deref(), f.dest.as_str()), (Some("a@one"), "a@one")),
            _ => panic!("the machine form"),
        }
        let _ = g.update(Message::MachineDest("a@uno".into()));
        let _ = g.update(Message::SaveMachine);
        let names: Vec<_> = g.saved.machines.iter().map(|m| m.dest.as_str()).collect();
        assert_eq!(names, ["b@two", "a@uno"], "renamed, not duplicated");
        // Forgetting drops it from the list and the file; connecting to it then fails with a toast.
        let _ = g.update(Message::ForgetMachine("b@two".into()));
        assert_eq!(hosts::load(&file).machines.len(), 1);
        let toasts = g.toasts.len();
        let _ = g.update(Message::UseMachine("b@two".into()));
        assert_eq!(g.toasts.len(), toasts + 1);
        assert!(g.toasts.last().is_some_and(|t| !t.ok && t.text.contains("b@two")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_size_survives_a_machine_switch() {
        let mut g = connected();
        assert_eq!(g.window, WINDOW);
        let _ = g.update(Message::Resized(iced::Size::new(1000.0, 600.0)));
        let _ = g.update(Message::UseThisMachine);
        let _ = g.update(Message::AddMachine);
        let _ = g.update(Message::MachineDest("deploy@web-1".into()));
        let _ = g.update(Message::SaveMachine);
        assert_eq!(g.window, iced::Size::new(1000.0, 600.0), "the window is the same after the switch");
    }

    #[test]
    fn a_dropdown_opens_closes_and_closes_on_a_choice() {
        let mut g = connected();
        let _ = g.update(Message::ToggleMenu(MenuKind::Restart));
        assert_eq!(g.menu, Some(MenuKind::Restart));
        let _ = g.update(Message::ToggleMenu(MenuKind::Machines));
        assert_eq!(g.menu, Some(MenuKind::Machines), "opening another replaces the first");
        let _ = g.update(Message::ToggleMenu(MenuKind::Machines));
        assert_eq!(g.menu, None, "the same button again closes it");
        let _ = g.update(Message::ToggleMenu(MenuKind::Restart));
        let _ = g.update(Message::CloseMenu);
        assert_eq!(g.menu, None);
        let _ = g.update(Message::ToggleMenu(MenuKind::Restart));
        let _ = g.update(Message::Select("web".into()));
        assert_eq!(g.menu, None, "choosing an app closes the menu");
        let _ = g.update(Message::ToggleMenu(MenuKind::Restart));
        let _ = g.update(Message::Act("web".into(), Act::RollingRestart));
        assert_eq!(g.menu, None, "and so does an action");
        let _ = g.update(Message::ToggleMenu(MenuKind::Machines));
        let _ = g.update(Message::OpenAdd);
        assert_eq!(g.menu, None);
    }

    #[test]
    fn the_app_filter_is_kept_across_a_machine_switch() {
        let mut g = connected();
        let _ = g.update(Message::AppFilter("we".into()));
        assert_eq!(g.filter, "we");
        let _ = g.update(Message::MachineDest("deploy@web-1".into())); // no form open: nothing happens
        let _ = g.update(Message::AddMachine);
        let _ = g.update(Message::MachineDest("deploy@web-1".into()));
        let _ = g.update(Message::SaveMachine);
        assert_eq!(g.filter, "we", "the filter box belongs to the window, not to a machine");
    }

    #[test]
    fn add_form_validates_before_running() {
        let mut g = connected();
        let _ = g.update(Message::OpenAdd);
        let _ = g.update(Message::AddWhat("server.js".into()));
        let _ = g.update(Message::AddName("my app".into()));
        let _ = g.update(Message::SubmitAdd);
        match &g.modal {
            Modal::Add(a) => assert!(matches!(&a.status, AddStatus::Done(Err(e)) if e.contains("name"))),
            _ => panic!("add form"),
        }
        let _ = g.update(Message::AddName("api2".into()));
        match &g.modal {
            Modal::Add(a) => assert_eq!(a.status, AddStatus::Editing, "editing clears the error"),
            _ => panic!("add form"),
        }
        let _ = g.update(Message::SubmitAdd);
        assert!(matches!(&g.modal, Modal::Add(a) if a.status == AddStatus::Running));
        let _ = g.update(Message::CloseModal);
        assert!(matches!(g.modal, Modal::Add(_)), "a running command keeps its dialog");
    }

    #[test]
    fn editor_flow() {
        let mut g = connected();
        let _ = g.update(Message::OpenEditor("api".into()));
        assert!(
            matches!(&g.modal, Modal::Editor(e) if e.path == "/etc/warden/api.toml" && e.status == EditorStatus::Loading)
        );
        let _ = g.update(Message::EditorLoaded { app: "api".into(), result: Ok("[app]\nname = \"api\"\n".into()) });
        let _ = g.update(Message::Edit(text_editor::Action::Edit(text_editor::Edit::Insert('#'))));
        assert!(matches!(&g.modal, Modal::Editor(e) if e.dirty && e.content.text().starts_with('#')));
        let _ = g.update(Message::Save);
        let _ = g.update(Message::Saved(Ok("api.toml: ok".into())));
        assert!(matches!(&g.modal, Modal::Editor(e) if e.saved && !e.dirty && e.status == EditorStatus::Ready));
        let _ = g.update(Message::ReloadAfterSave);
        assert!(matches!(g.modal, Modal::None));
        assert_eq!(g.busy, [("api".to_string(), Act::Reload)]);
        // No config known: an error toast, no editor.
        let _ = g.update(Message::OpenEditor("nope".into()));
        assert!(matches!(g.modal, Modal::None) && !g.toasts.last().unwrap().ok);
    }
}
