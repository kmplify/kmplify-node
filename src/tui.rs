//! `kmplify-node tui` — the terminal dashboard.
//!
//! A KMPLIFY node runs on machines that have no desktop: a Linux box in a
//! rack, a Mac mini on a shelf, a Windows workstation lending its card
//! overnight. The desktop app has a sharing screen for all of this; a headless
//! host had a log file and `journalctl`. This is that screen, in a terminal,
//! and it does the same two jobs:
//!
//! * **watch** — link state, what is advertised, what peers are running here,
//!   how much of the machine they hold, and the log;
//! * **control** — pause and resume sharing, evict a session, force a
//!   reconnect, stop the node.
//!
//! # Two ways to run it
//!
//! * **attached** (the common one) — a node is already running here under
//!   systemd, Docker or launchd. The dashboard reads the snapshot that node
//!   publishes and sends commands back through the control directory. Quitting
//!   leaves the node running.
//! * **standalone** — no node is running, so this process starts one and
//!   renders it. Quitting stops it, exactly like `kmplify-node` under Ctrl-C.
//!
//! Both read the SAME [`crate::status::Snapshot`], so the panels cannot say
//! one thing here and another over there.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Sparkline, Table, Wrap};
use ratatui::Frame;

use kmplify_node::control::Command;
use kmplify_node::fabric_worker::{self, WorkerConfig};
use kmplify_node::gpu::Backend;
use kmplify_node::peers::{self, Peers};
use kmplify_node::settings::Settings;
use kmplify_node::status::{self, Link, Snapshot};
use kmplify_node::views;
use serde_json::Value;

/// Repaint rate. Fast enough that a keypress feels immediate, slow enough that
/// the dashboard is invisible in the node's own CPU figure.
const FRAME: Duration = Duration::from_millis(250);

/// How often an attached dashboard re-reads the published snapshot. The node
/// writes it every [`status::PUBLISH_INTERVAL`]; reading faster only costs
/// syscalls.
const POLL: Duration = Duration::from_millis(1000);

/// Ceiling on a gateway call made from the dashboard (the peers screen). Long
/// enough for a busy gateway, short enough that a wedged one is obvious.
const GATEWAY_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the dashboard asks a rewards companion. Minutes, because a
/// balance is not a live metric and asking costs a process spawn.
const REWARDS_POLL: Duration = Duration::from_secs(120);

/// How often the peers screen re-asks while it is open. Consumers arrive and
/// leave on human timescales; polling harder would only add gateway load.
const PEERS_POLL: Duration = Duration::from_secs(5);

/// Palette, kept to the sixteen ANSI colours so the dashboard looks right on
/// whatever terminal a server happens to have, light or dark, and never paints
/// its own background.
///
/// Each measurement keeps ONE colour everywhere it appears — the same cyan for
/// CPU on the home meters, on its gauge, on its sparkline and on its per-core
/// grid. That is what makes four graphs on one screen readable at a glance
/// rather than four graphs that have to be labelled and read.
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const CPU_C: Color = Color::LightCyan;
const GPU_C: Color = Color::LightMagenta;
const VRAM_C: Color = Color::LightGreen;
const RAM_C: Color = Color::LightBlue;
const DISK_C: Color = Color::Yellow;

/// Samples kept per metric: five minutes at one a second, which is long
/// enough to show that the job that just ran was the spike.
const HISTORY: usize = 300;

/// A rolling window of one measurement.
#[derive(Default)]
struct Track {
    samples: std::collections::VecDeque<u64>,
    /// False once the platform has said it will not report this figure, so
    /// the panel can say so instead of drawing a flat line at zero.
    reported: bool,
}

impl Track {
    fn push(&mut self, value: Option<u64>) {
        let Some(v) = value else { return };
        self.reported = true;
        if self.samples.len() == HISTORY {
            self.samples.pop_front();
        }
        self.samples.push_back(v);
    }

    /// The tail that fits `width` cells, oldest first.
    fn window(&self, width: usize) -> Vec<u64> {
        let start = self.samples.len().saturating_sub(width.max(1));
        self.samples.iter().skip(start).copied().collect()
    }

    fn last(&self) -> Option<u64> {
        self.samples.back().copied()
    }

    /// Mean over the window, for the "busy lately?" line under each graph.
    fn mean(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().sum::<u64>() / self.samples.len() as u64)
    }

    fn peak(&self) -> Option<u64> {
        self.samples.iter().copied().max()
    }
}

/// Everything the activity screen graphs, sampled once a second.
#[derive(Default)]
struct Meters {
    cpu: Track,
    gpu: Track,
    vram: Track,
    ram: Track,
}

impl Meters {
    fn sample(&mut self, s: &Snapshot) {
        self.cpu.push(Some(s.cpu_percent.clamp(0.0, 100.0) as u64));
        self.gpu.push(s.gpu_percent.map(u64::from));
        self.vram.push(vram_used_percent(s));
        self.ram.push(percent(s.ram_used_mb, s.ram_total_mb));
    }
}

/// VRAM in use, as a percentage, or `None` where no such number exists.
///
/// Unified-memory backends have no distinct "used VRAM" to read, so the field
/// stays at zero — and a graph that plots that zero says "idle card" in a
/// place where the truth is "nobody can tell you". One rule, used by the
/// gauge, the graph and the history alike.
fn vram_used_percent(s: &Snapshot) -> Option<u64> {
    if !matches!(s.accelerator.as_str(), "cuda" | "rocm") {
        return None;
    }
    percent(s.vram_used_mb, s.vram_total_mb)
}

/// `used` as a percentage of `total`, or `None` when there is no total to be
/// a percentage of.
fn percent(used: u64, total: u64) -> Option<u64> {
    (total > 0).then(|| (used * 100 / total).min(100))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Sessions,
    Models,
    Logs,
    /// The desktop app's "Provide this machine's Resources" panel: what this
    /// machine lends, and how much of it.
    Sharing,
    /// Who may use it: waiting consumers, active ones, and invitations.
    Peers,
    /// The activity monitor: CPU, GPU, VRAM and RAM over time, per core, and
    /// what the fabric is holding.
    Activity,
    /// The web dashboard's "Trusted node" tab, from the same model.
    Trust,
    /// The web dashboard's "Earnings" tab, from the same model.
    Earnings,
    /// The web dashboard's "Settings" tab: every setting, its value, where it
    /// comes from and how to change it. The editor is the Sharing view.
    Settings,
}

/// Something a spawned background call has to say: the gateway about peers,
/// or an optional rewards companion about payouts.
enum BgMsg {
    Loaded(Result<Peers, String>),
    /// A decision or an invitation change: a line for the footer, and a
    /// reason to refetch.
    Acted(Result<String, String>),
    /// What a rewards companion reports, or why it could not be asked.
    Rewards(Result<kmplify_node::rewards::Report, String>),
}

/// A text setting being typed into.
struct Editing {
    key: &'static str,
    label: &'static str,
    buffer: String,
    /// Never echo a key to the screen; it is being typed on a machine
    /// somebody else can usually see.
    masked: bool,
}

/// A destructive action waiting for `y`.
#[derive(Clone)]
enum Confirm {
    Evict(String),
    Shutdown,
    /// Revoking is permanent: the consumer holding this invitation loses the
    /// contract and cannot get it back.
    Revoke(String),
}

impl Confirm {
    fn question(&self) -> String {
        match self {
            Confirm::Evict(id) => format!(
                "Stop session {} and remove its container?  [y/N]",
                &id[..12.min(id.len())]
            ),
            Confirm::Shutdown => {
                "Stop this node? Hosted sessions are torn down first.  [y/N]".into()
            }
            Confirm::Revoke(id) => format!(
                "Revoke invitation {}? The consumer holding it loses access permanently.  [y/N]",
                &id[..8.min(id.len())]
            ),
        }
    }
}

struct App {
    /// `None` while this process owns the worker: the snapshot is read
    /// straight out of it. `Some(dir)` when attached to another process.
    attached_to: Option<PathBuf>,
    /// Brought up by the node serving in this console (the terminal view of
    /// a running node): leaving returns to the console, the node keeps
    /// running, and "stop the node" asks the node to stop.
    embedded: bool,
    /// What every view shows ([`views::model`]), rebuilt from `snap` on each
    /// refresh: the Trusted node, Earnings and Settings screens render it.
    model: Value,
    /// The engine this node lends, for the model's sharing text.
    engine: String,
    node_dir: PathBuf,
    snap: Snapshot,
    view: View,
    selected: usize,
    /// Log pane scroll, counted from the bottom. 0 follows the tail.
    log_scroll: usize,
    confirm: Option<Confirm>,
    /// The sharing settings as edited here but not yet applied. Edits are a
    /// DRAFT on purpose: a ceiling is re-advertised by reconnecting, and
    /// reconnecting on every arrow key would make the node flap.
    draft: Settings,
    /// The draft as last saved, so "unsaved" is a fact rather than a guess.
    saved: Settings,
    sharing_sel: usize,
    /// The text field currently being typed into, if any.
    editing: Option<Editing>,
    /// Consumers and invitations, as the gateway last reported them. `None`
    /// until the first fetch answers.
    peers: Option<Peers>,
    peers_error: String,
    peers_sel: usize,
    peers_loading: bool,
    /// Rolling measurements behind the activity screen and the home meters.
    meters: Meters,
    /// What an optional rewards companion last said. `None` until asked, and
    /// nothing is asked unless the operator switched rewards on.
    rewards: Option<Result<kmplify_node::rewards::Report, String>>,
    last_rewards_ask: Instant,
    last_peers_fetch: Instant,
    /// Where gateway work reports back to. The screen must keep painting
    /// while a request is in flight, so every call is spawned and answers
    /// here.
    peer_tx: tokio::sync::mpsc::UnboundedSender<BgMsg>,
    /// The node's credential, for talking to the gateway as this node.
    creds_path: PathBuf,
    gateway: String,
    /// Last thing this dashboard did, shown in the footer.
    notice: String,
    notice_at: Instant,
    help: bool,
    quit: bool,
}

impl App {
    fn attached(&self) -> bool {
        self.attached_to.is_some()
    }

    fn refresh(&mut self) {
        self.snap = match &self.attached_to {
            Some(dir) => status::read_published(dir).unwrap_or_default(),
            None => status::snapshot(),
        };
        let stored = Settings::load(&self.node_dir);
        let attached = self.attached();
        self.model = views::model(&views::Source {
            snap: &self.snap,
            stored: &stored,
            engine: &self.engine,
            logs: if attached {
                self.snap.logs.clone()
            } else {
                status::logs()
            },
            rechecking: !attached && kmplify_node::security::rechecking(),
            view: if attached {
                kmplify_node::control::ViewState::default()
            } else {
                kmplify_node::control::view_state()
            },
        });
        // The running node's gateway is the authority: attached, this process
        // may have a different environment entirely, and asking the wrong
        // gateway about "my consumers" answers about a node that is not this
        // one.
        if !self.snap.gateway.is_empty() {
            self.gateway = self.snap.gateway.clone();
        }
        self.meters.sample(&self.snap);
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.notice = msg.into();
        self.notice_at = Instant::now();
    }

    /// Send a command to whichever node this dashboard is driving.
    fn send(&mut self, cmd: Command) {
        let outcome = match &self.attached_to {
            Some(dir) => kmplify_node::control::request(dir, &cmd),
            None => kmplify_node::control::submit(&cmd),
        };
        match outcome {
            Ok(()) => {
                let msg = cmd.confirmation();
                self.say(msg);
            }
            Err(e) => self.say(format!("could not send: {e}")),
        }
    }

    /// Write a plain-text report of everything on screen.
    ///
    /// The first thing anyone is asked for when a node misbehaves is what its
    /// dashboard said; a screenshot of a terminal is a poor way to send that,
    /// and this is a file.
    fn write_snapshot(&mut self) {
        // The same report the web dashboard's "Save a report" writes.
        match views::write_report(&self.node_dir, &self.snap) {
            Ok(path) => self.say(format!("wrote {}", path.display())),
            Err(e) => self.say(e),
        }
    }

    /// Ask the node serving in this console to bring up another view. Only
    /// the node's own process can open them: attached, there is no console
    /// of ours it could use, so this says where to go instead.
    fn open_view(&mut self, view: &str) {
        if self.attached() {
            let url = kmplify_node::dashboard::read_url_file(&self.node_dir);
            self.say(match url {
                Some(_) => {
                    "attached: run `kmplify-node status` here for the dashboard link".to_string()
                }
                None => {
                    "the node running here has no dashboard (started with --ui none)".to_string()
                }
            });
            return;
        }
        self.send(Command::View(view.to_string()));
    }

    fn rows(&self) -> usize {
        match self.view {
            View::Sessions => self.snap.sessions.len(),
            View::Models => self.snap.models.len(),
            // Scrolled text, not rows; the paragraph stops at its end.
            View::Trust | View::Settings => 200,
            _ => 0,
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Typing into a text field takes every key, or the country code "d"
        // would clear an override and "q" would quit the dashboard.
        if self.editing.is_some() {
            self.edit_key(key);
            return;
        }
        // A pending confirmation swallows everything else: no key should mean
        // "evict" by accident.
        if let Some(pending) = self.confirm.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm = None;
                    match pending {
                        Confirm::Evict(id) => self.send(Command::StopSession(id)),
                        Confirm::Revoke(id) => self.revoke_invitation(id),
                        // Standalone, this process IS the node and leaving
                        // the dashboard already tears it down; sending the
                        // command as well would ask the node to stop itself
                        // through a channel only a connected session reads.
                        Confirm::Shutdown => {
                            if self.attached() || self.embedded {
                                self.send(Command::Shutdown);
                            } else {
                                self.quit = true;
                            }
                        }
                    }
                }
                _ => {
                    self.confirm = None;
                    self.say("cancelled");
                }
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }

        // The sharing screen rebinds the keys that mean something different
        // there (space, arrows, d, s, esc); everything else falls through to
        // the global bindings below.
        if self.view == View::Sharing && self.on_sharing_key(key) {
            return;
        }
        if self.view == View::Peers && self.on_peers_key(key) {
            return;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') | KeyCode::Char('h') => self.view = View::Home,
            KeyCode::Char('2') | KeyCode::Char('s') => {
                self.view = View::Sessions;
                self.selected = 0;
            }
            KeyCode::Char('3') | KeyCode::Char('m') => {
                self.view = View::Models;
                self.selected = 0;
            }
            KeyCode::Char('4') | KeyCode::Char('l') => self.view = View::Logs,
            KeyCode::Char('5') | KeyCode::Char('g') => {
                self.view = View::Sharing;
                // Row 0 is a section heading.
                self.sharing_sel = self.sharing_sel.max(1);
            }
            KeyCode::Char('6') => {
                self.view = View::Peers;
                self.fetch_peers();
            }
            KeyCode::Char('7') | KeyCode::Char('t') => self.view = View::Activity,
            KeyCode::Char('r') => {
                self.refresh();
                self.say("refreshed");
            }
            KeyCode::Char('w') => self.write_snapshot(),
            KeyCode::Char('8') | KeyCode::Char('n') => self.view = View::Trust,
            KeyCode::Char('9') | KeyCode::Char('$') => self.view = View::Earnings,
            KeyCode::Char('0') => self.view = View::Settings,
            KeyCode::Char('R') => self.send(Command::Recheck),
            KeyCode::Char('b') => self.open_view("web"),
            KeyCode::Char('a') => self.open_view("app"),
            KeyCode::Char('p') => {
                if self.snap.paused {
                    self.send(Command::Resume);
                } else {
                    self.send(Command::Pause);
                }
            }
            KeyCode::Char('c') => self.send(Command::Reconnect),
            KeyCode::Char('e') => match self.snap.sessions.get(self.selected) {
                Some(s) if self.view == View::Sessions => {
                    self.confirm = Some(Confirm::Evict(s.session_id.clone()))
                }
                _ => self.say("open the sessions view and select one to evict"),
            },
            KeyCode::Char('x') => self.confirm = Some(Confirm::Shutdown),
            KeyCode::Up | KeyCode::Char('k') => match self.view {
                View::Logs => self.log_scroll = self.log_scroll.saturating_add(1),
                _ => self.selected = self.selected.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.view {
                View::Logs => self.log_scroll = self.log_scroll.saturating_sub(1),
                _ => {
                    let last = self.rows().saturating_sub(1);
                    self.selected = (self.selected + 1).min(last);
                }
            },
            _ => {}
        }
    }
}

/// Entry point for `kmplify-node tui`.
pub async fn main(cfg: WorkerConfig, dir: PathBuf, attach: bool, standalone: bool) -> i32 {
    // Checked before anything is started: without a terminal, entering the
    // alternate screen fails, and in standalone mode that would leave a node
    // running behind a panic message.
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "`kmplify-node tui` needs a terminal.\n\
             For a script or a redirected shell use `kmplify-node status --json`, \
             or `kmplify-node` to run the node with plain logs."
        );
        return 2;
    }
    // A service-managed node publishes as its own user. Reading its snapshot
    // as somebody else fails with a permission error, and treating that as
    // "no node is running" would start a second worker against a machine that
    // already has one.
    let live = match status::read_published_result(&dir) {
        Ok(snap) => snap.filter(Snapshot::is_fresh),
        Err(e) => {
            eprintln!(
                "cannot read {}: {e}\n\
                 A node is installed here but publishes as another user. Attach as that \
                 user, e.g.\n\
                 \x20 sudo -u kmplify KMPLIFY_NODE_DIR={} kmplify-node tui",
                status::status_path(&dir).display(),
                dir.display()
            );
            return 1;
        }
    };
    let attach_mode = match (attach, standalone) {
        (true, _) => {
            if live.is_none() {
                eprintln!(
                    "no node is running in {} — start one, or drop --attach to run one here",
                    dir.display()
                );
                return 1;
            }
            true
        }
        (_, true) => false,
        // Attaching to a running node is the polite default: starting a second
        // worker against the same credential file would have two processes
        // answering for one node id.
        _ => live.is_some(),
    };

    // Captured before the config is handed to a worker: the peers screen
    // needs both to talk to the gateway as this node, and the activity
    // screen needs to know which accelerator to ask about.
    let creds_path = cfg.creds_path.clone();
    let gateway = cfg.gateway_url.clone();
    let accel = cfg.accel();
    let engine = cfg.ollama_base.clone();

    let node = if attach_mode {
        None
    } else {
        // The dashboard owns the terminal from here on, so worker logs go to
        // the ring instead of over the frame.
        status::set_quiet(true);
        Some(crate::start_node(cfg, dir.clone()).await)
    };

    let app = new_app(
        dir,
        attach_mode,
        false,
        creds_path,
        gateway,
        engine,
        live.unwrap_or_default(),
    );
    let code = run(app, accel, None).await;
    status::set_quiet(false);

    if let Some(node) = node {
        println!("[kmplify-node] stopping — tearing down hosted sessions…");
        node.shutdown().await;
        println!("[kmplify-node] stopped cleanly");
    }
    code
}

/// A dashboard App for `dir`: attached to another process, the node of
/// this process (`kmplify-node tui` standalone), or the terminal view of the
/// node serving in this console (`embedded`).
fn new_app(
    dir: PathBuf,
    attach_mode: bool,
    embedded: bool,
    creds_path: PathBuf,
    gateway: String,
    engine: String,
    snap: Snapshot,
) -> (App, tokio::sync::mpsc::UnboundedReceiver<BgMsg>) {
    // The stored sharing choices are the starting point of the draft, so the
    // screen opens showing what this node is actually applying.
    let stored = Settings::load(&dir);
    let (peer_tx, peer_rx) = tokio::sync::mpsc::unbounded_channel::<BgMsg>();
    let app = App {
        attached_to: attach_mode.then(|| dir.clone()),
        embedded,
        model: Value::Null,
        engine,
        node_dir: dir,
        draft: stored.clone(),
        saved: stored,
        sharing_sel: 1,
        editing: None,
        peers: None,
        peers_error: String::new(),
        peers_sel: 0,
        peers_loading: false,
        meters: Meters::default(),
        rewards: None,
        last_rewards_ask: Instant::now() - REWARDS_POLL,
        last_peers_fetch: Instant::now(),
        peer_tx,
        creds_path,
        gateway,
        snap,
        view: View::Home,
        selected: 0,
        log_scroll: 0,
        confirm: None,
        notice: if embedded {
            "terminal view of this node · q back to the console · b web · a app window".into()
        } else if attach_mode {
            "attached to the node running here".into()
        } else {
            "started a node in this terminal".into()
        },
        notice_at: Instant::now(),
        help: false,
        quit: false,
    };

    (app, peer_rx)
}

/// Draw and drive `app` until it quits, or until `quit` turns true.
async fn run(
    (mut app, mut peer_rx): (App, tokio::sync::mpsc::UnboundedReceiver<BgMsg>),
    accel: Backend,
    quit: Option<tokio::sync::watch::Receiver<bool>>,
) -> i32 {
    // Keys are read on a blocking thread: crossterm's reader is blocking, and
    // an async runtime thread parked in it would stall every task on it.
    let (keys_tx, mut keys_rx) = tokio::sync::mpsc::unbounded_channel::<KeyEvent>();
    let reader = std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => match event::read() {
                // Windows reports press AND release; acting on both would
                // toggle pause twice per keystroke.
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    if keys_tx.send(k).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            },
            Ok(false) => {
                if keys_tx.is_closed() {
                    return;
                }
            }
            Err(_) => return,
        }
    });

    let mut terminal = ratatui::init();
    let mut last_poll = Instant::now() - POLL;
    let mut ticks: u32 = 0;
    let code = loop {
        if last_poll.elapsed() >= POLL {
            // When this process IS the node, sample for the screen at the
            // rate someone watching expects rather than at the publisher's
            // slower file-writing cadence. The GPU probe is a subprocess, so
            // it still goes every other second.
            if !app.attached() {
                status::sample_host(accel, ticks.is_multiple_of(2)).await;
            }
            app.refresh();
            ticks = ticks.wrapping_add(1);
            last_poll = Instant::now();
        }
        if terminal.draw(|f| draw(f, &app)).is_err() {
            break 1;
        }
        if app.quit || quit.as_ref().is_some_and(|q| *q.borrow()) {
            break 0;
        }
        // While the peers screen is open, keep it current: consumers arrive
        // and leave without this dashboard doing anything.
        if app.view == View::Peers
            && !app.peers_loading
            && app.last_peers_fetch.elapsed() >= PEERS_POLL
        {
            app.fetch_peers();
        }
        if app.last_rewards_ask.elapsed() >= REWARDS_POLL {
            app.fetch_rewards();
        }
        tokio::select! {
            key = keys_rx.recv() => match key {
                Some(k) => app.on_key(k),
                // The reader thread died; without input this is a very
                // expensive `status` command.
                None => break 1,
            },
            msg = peer_rx.recv() => {
                if let Some(msg) = msg {
                    app.on_bg_msg(msg);
                }
            }
            _ = tokio::time::sleep(FRAME) => {}
        }
    };
    ratatui::restore();
    drop(keys_rx);
    let _ = reader.join();

    code
}

/// The terminal view of the node serving in THIS process, brought up in its
/// console while the node keeps running (`--ui tui`, the chooser's
/// "Terminal view", the t key, or the dashboard's View buttons). Leaving it
/// returns to the plain console; `quit` closes it when the node stops.
pub async fn embedded(
    dir: PathBuf,
    creds_path: PathBuf,
    gateway: String,
    engine: String,
    accel: Backend,
    quit: tokio::sync::watch::Receiver<bool>,
) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    kmplify_node::control::set_terminal_up(true);
    status::set_quiet(true);
    // The console key reader polls in 100 ms steps and stands aside while
    // this view is up; let its last poll finish before this one reads keys.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let app = new_app(
        dir,
        false,
        true,
        creds_path,
        gateway,
        engine,
        status::snapshot(),
    );
    let stopping = *quit.borrow();
    run(app, accel, Some(quit)).await;
    status::set_quiet(false);
    kmplify_node::control::set_terminal_up(false);
    if !stopping {
        println!(
            "[kmplify-node] back in the console; the node keeps running. \
             t terminal view, w web view, a app window (type the letter, then Enter)"
        );
    }
}

// ------------------------------------------------------------- console
//
// Plain console input, outside the terminal view: the start-up chooser and
// the view keys. Read through crossterm's event reader WITHOUT raw mode, so
// the console keeps its line editing and echo (on Windows a key arrives as
// it is pressed, elsewhere when Enter is pressed), Ctrl-C stays a signal,
// and the terminal view can take the same reader over while it is up.

/// Ask which view to start with; `None` when there is no answer within
/// `wait` (the caller keeps its default) or the console cannot be read.
pub fn ask_start_view(default: &str, wait: Duration) -> Option<views::Choice> {
    use std::io::Write;
    println!("\n{}", views::CHOOSER_TEXT);
    let deadline = Instant::now() + wait;
    let mut line = String::new();
    loop {
        print!(
            "Your choice (no answer in {} s starts the {default} view): ",
            deadline.saturating_duration_since(Instant::now()).as_secs()
        );
        let _ = std::io::stdout().flush();
        line.clear();
        let entered = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break false;
            }
            match event::poll(left.min(Duration::from_millis(200))) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                        KeyCode::Enter => break true,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return None
                        }
                        KeyCode::Char(c) => {
                            line.push(c);
                            if cfg!(windows) {
                                print!("{c}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                        KeyCode::Backspace => {
                            line.pop();
                            if cfg!(windows) {
                                print!("\u{8} \u{8}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(_) => return None,
                },
                Ok(false) => {}
                Err(_) => return None,
            }
        };
        if !entered {
            println!("\nno answer: starting the {default} view");
            return None;
        }
        if cfg!(windows) {
            println!();
        }
        match views::parse_choice(&line) {
            Ok(c) => return c,
            Err(e) => println!("{e}"),
        }
    }
}

/// The view keys of the plain console: each key pressed (letters only),
/// while the terminal view is not up. Ends when the receiver is dropped.
pub fn console_keys() -> tokio::sync::mpsc::UnboundedReceiver<char> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        if tx.is_closed() {
            return;
        }
        if kmplify_node::control::terminal_up() {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {
                if let Ok(Event::Key(k)) = event::read() {
                    if k.kind == KeyEventKind::Press {
                        if let KeyCode::Char(c) = k.code {
                            if tx.send(c.to_ascii_lowercase()).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(_) => return,
        }
    });
    rx
}

// ------------------------------------------------------------------ render

fn draw(f: &mut Frame, app: &App) {
    let [header, sub, trust, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(header_line(app), header);
    f.render_widget(sub_line(app), sub);
    f.render_widget(trust_line(&app.snap), trust);
    match app.view {
        View::Home => draw_home(f, app, body),
        View::Sessions => draw_sessions(f, app, body),
        View::Models => draw_models(f, app, body),
        View::Logs => draw_logs(f, app, body),
        View::Sharing => draw_sharing(f, app, body),
        View::Peers => draw_peers(f, app, body),
        View::Activity => draw_activity(f, app, body),
        View::Trust => draw_trust(f, app, body),
        View::Earnings => draw_earnings(f, app, body),
        View::Settings => draw_settings(f, app, body),
    }
    f.render_widget(footer_line(app), footer);

    if app.help {
        draw_overlay(f, "keys", help_text());
    }
    if let Some(c) = &app.confirm {
        draw_overlay(f, "confirm", c.question());
    }
}

fn header_line(app: &App) -> Paragraph<'static> {
    let title = match app.view {
        View::Home => "live dashboard",
        View::Sessions => "sessions",
        View::Models => "models",
        View::Logs => "log",
        View::Sharing => "sharing",
        View::Peers => "peers",
        View::Activity => "activity",
        View::Trust => "trusted node",
        View::Earnings => "earnings",
        View::Settings => "settings",
    };
    Paragraph::new(Line::from(vec![
        Span::styled(" ◆ KMPLIFY", Style::new().fg(GPU_C).bold()),
        Span::styled("  kmplify-node", Style::new().fg(Color::White).bold()),
        Span::styled(
            "   provider · compute fabric · inference",
            Style::new().fg(MUTED),
        ),
        Span::raw("   "),
        Span::styled(title, Style::new().fg(view_colour(app.view)).bold()),
    ]))
}

/// The trust readiness meter in one line: the bar, coloured by band, and
/// the verdict in the same words as the dashboard and `status`.
fn trust_line(s: &Snapshot) -> Paragraph<'static> {
    let progress = s.security_progress.clone().unwrap_or_default();
    if s.security.is_none() && !progress.running {
        return Paragraph::new(Line::from(Span::styled(
            " trust     security check not published yet",
            Style::new().fg(MUTED),
        )));
    }
    let r = kmplify_node::trust::readiness(
        s.security.as_ref(),
        s.fabric_trust.as_ref(),
        &progress,
        kmplify_node::identity::now_s(),
    );
    let colour = match r.band {
        "green" => Color::Green,
        "amber" => Color::Yellow,
        "red" => Color::Red,
        _ => MUTED,
    };
    Paragraph::new(Line::from(vec![
        Span::styled(" trust     ", Style::new().fg(MUTED)),
        Span::styled(
            kmplify_node::trust::readiness_bar(&r),
            Style::new().fg(colour).bold(),
        ),
        Span::raw("  "),
        Span::styled(r.verdict, Style::new().fg(Color::White)),
    ]))
}

/// The "is this thing on" line: state, identity, and how stale the reading is.
fn sub_line(app: &App) -> Paragraph<'static> {
    let s = &app.snap;
    let (label, colour) = link_style(app);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(label, Style::new().fg(colour).bold()),
    ];
    if s.paused {
        spans.push(Span::styled(
            " PAUSED",
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    let node = if s.node_id.is_empty() {
        "registering…".to_string()
    } else {
        s.node_id[..12.min(s.node_id.len())].to_string()
    };
    spans.push(sep());
    spans.push(Span::styled("node ", Style::new().fg(MUTED)));
    spans.push(Span::styled(node, Style::new().fg(Color::White)));
    spans.push(sep());
    spans.push(Span::styled(
        gateway_host(&s.gateway),
        Style::new().fg(ACCENT),
    ));
    spans.push(sep());
    spans.push(Span::styled(
        format!("up {}", human(s.uptime())),
        Style::new().fg(Color::White),
    ));
    if s.reconnects > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{} reconnects", s.reconnects),
            Style::new().fg(Color::Yellow),
        ));
    }
    spans.push(sep());
    spans.push(Span::styled(
        if app.attached() {
            format!("attached to pid {}", s.pid)
        } else {
            "running here".to_string()
        },
        Style::new().fg(MUTED),
    ));
    spans.push(sep());
    spans.push(Span::styled(
        format!("refreshed {}", status::clock_hms(status::now_ms())),
        Style::new().fg(MUTED),
    ));
    Paragraph::new(Line::from(spans))
}

/// Each screen has a colour, and its panels and title share it — so a glance
/// at the top says which screen this is without reading the word.
fn view_colour(view: View) -> Color {
    match view {
        View::Home => Color::Yellow,
        View::Sessions => Color::LightBlue,
        View::Models => VRAM_C,
        View::Logs => MUTED,
        View::Sharing => GPU_C,
        View::Peers => ACCENT,
        View::Activity => CPU_C,
        View::Trust => Color::Green,
        View::Earnings => Color::Yellow,
        View::Settings => GPU_C,
    }
}

fn link_style(app: &App) -> (String, Color) {
    let s = &app.snap;
    // An attached dashboard can outlive the node it was watching; saying
    // ONLINE from a snapshot nobody is updating any more is the one lie this
    // screen must never tell.
    if app.attached() && !s.is_fresh() {
        return ("NO NODE".into(), Color::Red);
    }
    match s.link {
        Link::Online if s.paused => ("ONLINE".into(), Color::Yellow),
        Link::Online => ("ONLINE".into(), Color::Green),
        Link::Connecting | Link::Starting => (s.link.label().into(), Color::Yellow),
        Link::Retrying => ("RETRYING".into(), Color::Red),
        Link::Stopping | Link::Stopped => (s.link.label().into(), Color::Red),
    }
}

fn footer_line(app: &App) -> Paragraph<'static> {
    let quit = if app.attached() {
        "q quit"
    } else if app.embedded {
        "q console"
    } else {
        "q quit (stops node)"
    };
    let pause = if app.snap.paused {
        "p resume"
    } else {
        "p pause"
    };
    let keys = if app.view == View::Sharing {
        format!(
            " {quit}   1 home  2 sessions  3 models  4 log  5 sharing  6 peers   space toggle  ←/→ ceiling  d environment  s apply  ? keys"
        )
    } else if app.view == View::Peers {
        format!(
            " {quit}   1 home … 5 sharing  6 peers   a approve  n deny  b block  u clear  i invite  h hold  v revoke  ? keys"
        )
    } else {
        format!(
            " {quit}   1 overview 8 trusted 2 sessions 9 earnings 0 settings 4 log   {pause}  c reconnect  R re-check  b web  a app  x stop node  ? keys"
        )
    };
    // The notice replaces the key hints for a few seconds after an action, so
    // a command that failed is impossible to miss.
    let text = if app.notice_at.elapsed() < Duration::from_secs(4) && !app.notice.is_empty() {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(app.notice.clone(), Style::new().fg(Color::Yellow).bold()),
        ])
    } else {
        Line::from(Span::styled(keys, Style::new().fg(MUTED)))
    };
    Paragraph::new(text)
}

fn draw_home(f: &mut Frame, app: &App, area: Rect) {
    let [top, middle, logs] = Layout::vertical([
        Constraint::Length(9),
        Constraint::Min(12),
        Constraint::Length(7),
    ])
    .areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Min(30)]).areas(top);
    draw_machine(f, app, left);
    draw_work(f, app, right);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Min(30)]).areas(middle);
    let [trust, conn] = Layout::vertical([Constraint::Length(5), Constraint::Min(4)]).areas(left);
    f.render_widget(trust_panel(app), trust);
    f.render_widget(connection_panel(app), conn);
    let [models, sessions] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Min(3)]).areas(right);
    f.render_widget(models_table(app, 0), models);
    f.render_widget(sessions_table(app, false), sessions);
    f.render_widget(log_panel(app, logs.height.saturating_sub(2) as usize), logs);
}

/// What this machine is, and how much of it is currently spoken for.
/// The home screen's live meters: one row per measurement, each with its own
/// colour, its bar, and the last minute of history beside it.
///
/// The same four measurements the activity screen graphs, at a glance, so
/// the home screen answers "is anything happening" without a keystroke.
fn draw_machine(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.snap;
    let block = panel_coloured("this machine", CPU_C);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, cpu, gpu, vram, ram, tail] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let accel = accel_name(s);
    let name = if s.gpu_name.is_empty() {
        "no accelerator detected".to_string()
    } else {
        format!("{} · {} MB", s.gpu_name, s.vram_total_mb)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{accel:<7}"), Style::new().fg(GPU_C).bold()),
            Span::styled(name, Style::new().fg(Color::White)),
        ])),
        head,
    );
    meter_row(f, cpu, "cpu", CPU_C, &app.meters.cpu, {
        let lent = if s.reserved_cpus > 0.0 {
            s.reserved_cpus
        } else {
            0.0
        };
        format!("{lent:.0} of {:.0} cores lent", s.cpus)
    });
    meter_row(
        f,
        gpu,
        "gpu",
        GPU_C,
        &app.meters.gpu,
        if app.meters.gpu.reported {
            "busy".to_string()
        } else {
            "usage not reported here".to_string()
        },
    );
    meter_row(
        f,
        vram,
        "vram",
        VRAM_C,
        &app.meters.vram,
        if app.meters.vram.reported {
            format!("{} / {} MB", s.vram_used_mb, s.vram_total_mb)
        } else {
            format!("{} MB on the card", s.vram_total_mb)
        },
    );
    meter_row(
        f,
        ram,
        "ram",
        RAM_C,
        &app.meters.ram,
        format!("{} / {} GB", s.ram_used_mb / 1024, s.ram_total_mb / 1024),
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            s.cpu_model.clone(),
            Style::new().fg(MUTED),
        )))
        .wrap(Wrap { trim: true }),
        tail,
    );
}

/// `cpu  ████░░░░  38%  0 of 12 cores lent      ▂▃▅▂▁`
fn meter_row(f: &mut Frame, area: Rect, label: &str, colour: Color, track: &Track, note: String) {
    // The history strip is worth having only once there is room for the
    // numbers first; a squeezed terminal keeps the reading and loses the
    // decoration.
    let spark = if area.width > 60 { area.width / 4 } else { 0 };
    let [text, history] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(spark)]).areas(area);
    let Some(pct) = track.last() else {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {label:<5}"), Style::new().fg(colour)),
                Span::styled(note, Style::new().fg(MUTED)),
            ])),
            text,
        );
        return;
    };
    let filled = (pct as usize * 12 / 100).min(12);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {label:<5}"), Style::new().fg(colour)),
            Span::styled(
                "█".repeat(filled),
                Style::new().fg(load_colour(pct, colour)),
            ),
            Span::styled("·".repeat(12 - filled), Style::new().fg(MUTED)),
            Span::styled(format!(" {pct:>3}%  "), Style::new().fg(colour).bold()),
            Span::styled(note, Style::new().fg(MUTED)),
        ])),
        text,
    );
    if spark > 0 {
        f.render_widget(
            Sparkline::default()
                .data(track.window(history.width as usize))
                .max(100)
                .style(Style::new().fg(colour)),
            history,
        );
    }
}

/// What the fabric is getting out of it.
fn draw_work(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.snap;
    let block = panel_coloured("what this node shares", GPU_C);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(vec![
            field("inference"),
            onoff(s.share_inference && !s.paused),
            Span::styled(
                format!("  {} model(s) advertised", s.models.len()),
                Style::new().fg(VRAM_C),
            ),
        ]),
        Line::from(vec![
            field("cpu/ram"),
            onoff(s.share_cpu),
            field("sessions"),
            if s.workloads.is_empty() {
                Span::styled("off", Style::new().fg(MUTED))
            } else {
                Span::styled(s.workloads.join(","), Style::new().fg(Color::Green))
            },
        ]),
        Line::from(vec![
            field("jobs"),
            Span::styled(
                format!("{:<10}", format!("{} active", s.jobs.active)),
                Style::new().fg(if s.jobs.active > 0 {
                    Color::Green
                } else {
                    MUTED
                }),
            ),
            Span::styled(
                format!("{} finished  ", s.jobs.done),
                Style::new().fg(ACCENT),
            ),
            Span::styled(
                format!("{} errors  ", s.jobs.failed),
                Style::new().fg(if s.jobs.failed > 0 { Color::Red } else { MUTED }),
            ),
            Span::styled(format!("avg {} ms", s.jobs.avg_ms), Style::new().fg(MUTED)),
        ]),
        Line::from(vec![
            field("last"),
            if s.jobs.last_model.is_empty() {
                Span::styled("nothing yet", Style::new().fg(MUTED))
            } else {
                Span::styled(
                    format!("{} in {} ms", s.jobs.last_model, s.jobs.last_ms),
                    Style::new().fg(Color::White),
                )
            },
        ]),
        Line::from(vec![
            field("admission"),
            Span::styled(
                format!(
                    "{:<10}",
                    if s.approval_mode.is_empty() {
                        "auto"
                    } else {
                        &s.approval_mode
                    }
                ),
                Style::new().fg(if s.approval_mode == "manual" {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ),
            field("country"),
            if s.country.is_empty() {
                Span::styled("XX (undeclared)", Style::new().fg(MUTED))
            } else {
                Span::styled(s.country.clone(), Style::new().fg(Color::White))
            },
        ]),
    ];
    if s.functions_enabled || s.vectors_enabled {
        lines.push(Line::from(vec![
            field("lanes"),
            Span::raw(format!(
                "functions {} ({} calls)   vectors {} ({} MB of {})",
                if s.functions_enabled { "on" } else { "off" },
                s.jobs.functions,
                if s.vectors_enabled { "on" } else { "off" },
                s.vectors_used_mb,
                s.vectors_max_mb
            )),
        ]));
    }
    if let Some(credits) = s.credits.as_ref().filter(|c| c.enabled) {
        lines.push(Line::from(vec![
            field("credits"),
            Span::styled(
                kmplify_node::credits::summary_short(credits),
                Style::new().fg(if credits.linked {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
        ]));
    }
    if let Some(rewards) = &app.rewards {
        lines.push(Line::from(vec![
            field("rewards"),
            match rewards {
                Ok(r) => Span::styled(
                    kmplify_node::rewards::summary_short(r),
                    Style::new().fg(if r.testnet {
                        // A test-network balance is not money, and the colour
                        // says so before the number does.
                        Color::Yellow
                    } else if r.linked {
                        Color::Green
                    } else {
                        MUTED
                    }),
                ),
                Err(e) => Span::styled(e.clone(), Style::new().fg(MUTED)),
            },
        ]));
    }
    if !s.link_detail.is_empty() && s.link != Link::Online {
        lines.push(Line::from(vec![
            field("last err"),
            Span::styled(s.link_detail.clone(), Style::new().fg(Color::Red)),
        ]));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let [table, since, note] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(5),
        Constraint::Length(3),
    ])
    .areas(area);
    f.render_widget(sessions_table(app, true), table);
    let s = &app.snap;
    let jobs = if s.jobs.failed > 0 {
        format!("{} ({} failed)", s.jobs.done, s.jobs.failed)
    } else {
        s.jobs.done.to_string()
    };
    let rows = vec![
        Line::from(vec![
            field("sessions"),
            Span::raw(format!("{} hosted", s.delivered.sessions)),
            sep(),
            field("machine"),
            Span::raw(human(Duration::from_secs(s.delivered.session_seconds))),
        ]),
        Line::from(vec![
            field("chat jobs"),
            Span::raw(jobs),
            sep(),
            field("running"),
            Span::raw(s.jobs.active.to_string()),
        ]),
        Line::from(vec![
            field("functions"),
            Span::raw(s.delivered.functions.to_string()),
            sep(),
            field("vectors"),
            Span::raw(s.delivered.vector_ops.to_string()),
        ]),
    ];
    f.render_widget(Paragraph::new(rows).block(panel("since this start")), since);
    let text = if app.snap.sessions.is_empty() {
        "No peer is running a container here. Sessions are opt-in: PROVIDER_WORKLOADS lists the templates this node accepts."
    } else {
        "↑/↓ select · e stop session: the selected one (its container is stopped and removed)"
    };
    f.render_widget(
        Paragraph::new(text)
            .style(Style::new().fg(MUTED))
            .wrap(Wrap { trim: true })
            .block(panel("about")),
        note,
    );
}

fn draw_models(f: &mut Frame, app: &App, area: Rect) {
    let [table, note] = Layout::vertical([Constraint::Min(4), Constraint::Length(3)]).areas(area);
    f.render_widget(models_table(app, area.height as usize), table);
    let text = if app.snap.paused {
        "Paused: the gateway has been told this node serves nothing. Press p to resume."
    } else if app.snap.models.is_empty() {
        "Nothing advertised. The node lists what its model server answers with; pull a model, or check OLLAMA_BASE."
    } else {
        "What consumers can ask this node for. The list is refreshed from the model server about once a ping."
    };
    f.render_widget(
        Paragraph::new(text)
            .style(Style::new().fg(MUTED))
            .wrap(Wrap { trim: true })
            .block(panel("about")),
        note,
    );
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(log_panel(app, area.height.saturating_sub(2) as usize), area);
}

fn log_panel(app: &App, rows: usize) -> Paragraph<'static> {
    // In-process the ring is authoritative and complete; attached, the tail
    // the node published is all there is.
    let lines: Vec<String> = if app.attached() {
        app.snap.logs.clone()
    } else {
        status::logs()
    };
    let end = lines.len().saturating_sub(app.log_scroll);
    let start = end.saturating_sub(rows.max(1));
    let shown: Vec<Line> = lines[start..end]
        .iter()
        .map(|l| {
            // The timestamp is scaffolding; the message is the news. Split so
            // the eye lands on the second half.
            match l.split_once(' ') {
                Some((stamp, msg)) => Line::from(vec![
                    Span::styled(format!("{stamp} "), Style::new().fg(MUTED)),
                    Span::styled(msg.to_string(), log_style(msg)),
                ]),
                None => Line::from(Span::styled(l.clone(), log_style(l))),
            }
        })
        .collect();
    let title = if app.log_scroll > 0 {
        format!(
            "last log lines  (scrolled {} back, ↓ to follow)",
            app.log_scroll
        )
    } else {
        "last log lines".to_string()
    };
    Paragraph::new(shown).block(panel_coloured(&title, view_colour(View::Logs)))
}

/// Red for a line that reports a failure, yellow for one that reports a
/// refusal or a retry. Cheap and worth it: the log pane is four lines tall on
/// the home view and this is what makes the bad one findable.
fn log_style(line: &str) -> Style {
    let l = line.to_ascii_lowercase();
    if l.contains("failed") || l.contains("error") || l.contains("cannot") || l.contains("rejected")
    {
        Style::new().fg(Color::Red)
    } else if l.contains("refused") || l.contains("retry") || l.contains("lost") {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    }
}

fn sessions_table(app: &App, selectable: bool) -> Table<'static> {
    let now = status::now_ms() / 1000;
    let rows: Vec<Row> = app
        .snap
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let age = Duration::from_secs((now as i64 - s.since).max(0) as u64);
            let style = if selectable && i == app.selected {
                Style::new().fg(Color::Black).bg(ACCENT)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(s.session_id[..12.min(s.session_id.len())].to_string()),
                Cell::from(Span::styled(s.template.clone(), Style::new().fg(ACCENT))),
                Cell::from(state_span(&s.state)),
                Cell::from(Span::styled(
                    format!("{:.1}", s.cpus),
                    Style::new().fg(CPU_C),
                )),
                Cell::from(Span::styled(human(age), Style::new().fg(MUTED))),
            ])
            .style(style)
        })
        .collect();
    let title = format!("running now ({})", app.snap.sessions.len());
    Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Min(12),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(vec!["session", "template", "state", "cpus", "age"])
            .style(Style::new().fg(MUTED).add_modifier(Modifier::BOLD)),
    )
    .block(panel_coloured(&title, view_colour(View::Sessions)))
}

fn state_span(state: &str) -> Span<'static> {
    let colour = match state {
        "running" => Color::Green,
        "pulling" | "starting" => Color::Yellow,
        _ => MUTED,
    };
    Span::styled(state.to_string(), Style::new().fg(colour))
}

fn models_table(app: &App, _height: usize) -> Table<'static> {
    let s = &app.snap;
    let rows: Vec<Row> = s
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let engine = s.engines.get(m).cloned().unwrap_or_else(|| "local".into());
            // Colibri models stream from NVMe rather than sitting in VRAM;
            // the colour says which upstream a consumer would actually hit.
            let engine_style = if engine == "local" {
                Style::new().fg(VRAM_C)
            } else {
                Style::new().fg(GPU_C)
            };
            let style = if app.view == View::Models && i == app.selected {
                Style::new().fg(Color::Black).bg(ACCENT)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(m.clone()),
                Cell::from(Span::styled(engine, engine_style)),
            ])
            .style(style)
        })
        .collect();
    let title = if s.paused {
        "models (paused — nothing advertised)".to_string()
    } else {
        format!("models ({})", s.models.len())
    };
    Table::new(rows, [Constraint::Min(14), Constraint::Length(8)])
        .header(
            Row::new(vec!["model", "engine"])
                .style(Style::new().fg(MUTED).add_modifier(Modifier::BOLD)),
        )
        .block(panel_coloured(&title, view_colour(View::Models)))
}

fn draw_overlay(f: &mut Frame, title: &str, body: String) {
    let area = centred(f.area(), 62, 9);
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::Yellow))
                    .title(format!(" {title} ")),
            ),
        area,
    );
}

fn help_text() -> String {
    "1/h overview  8/n trusted node  2/s sessions  9/$ earnings  0 settings  4/l log\n\
     3/m models  5/g sharing editor  6 peers  7/t activity\n\
     ↑/↓ or j/k   move (log: scroll back)\n\
     p  pause sharing / resume sharing: stays connected, advertises nothing\n\
     c  reconnect now   ·   R  re-check now (security baseline)\n\
     e  stop session: the selected one (sessions view)\n\
     x  stop the node   ·   w  save a report (a text file)   ·   r refresh\n\
     b  web: the dashboard in the browser   ·   a  app window\n\
     5  change a setting   ·   d  use environment (in the editor)\n\
     q  leave: back to the console when the node runs here as a view,\n\
        otherwise stops the node only if it started here\n\
     \n\
     sharing (5): space toggles, ←/→ moves a ceiling (shift for a bigger\n\
     step), enter edits a field, d hands one back to the environment,\n\
     s applies — which reconnects so the fabric hears the new terms.\n\
     \n\
     peers (6): a approve, n deny, b block, u clear the standing rule,\n\
     i mint an invitation, h hold or resume one, v revoke it.\n\
     \n\
     activity (7): CPU, GPU, VRAM and RAM live, with five minutes of\n\
     history and a bar per core. Figures the platform will not report say\n\
     so rather than drawing a zero."
        .into()
}

// ------------------------------------------------------------------ pieces

fn panel(title: &'static str) -> Block<'static> {
    panel_owned(title.to_string())
}

fn panel_owned(title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        ))
}

/// A dim, fixed-width label, so the values after it line up into a column
/// instead of starting wherever the word happened to end.
fn field(name: &'static str) -> Span<'static> {
    Span::styled(format!("{name:<10}"), Style::new().fg(MUTED))
}

/// Padded to a column so whatever follows starts in the same place whether
/// the answer is "on" or "off".
fn onoff(on: bool) -> Span<'static> {
    if on {
        Span::styled("on   ", Style::new().fg(Color::Green).bold())
    } else {
        Span::styled("off  ", Style::new().fg(MUTED))
    }
}

fn sep() -> Span<'static> {
    Span::styled("  ·  ", Style::new().fg(MUTED))
}

fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// A URL's host, which is all the header line has room for.
fn gateway_host(url: &str) -> String {
    if url.is_empty() {
        return "no gateway".into();
    }
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

// ---------------------------------------------------------------- sharing

/// One line of the sharing screen.
///
/// A hand-rolled row model rather than a widget per control: the screen mixes
/// switches, a template checklist, sliders and text fields, and every one of
/// them needs the same three annotations — the value, where it came from
/// (environment or set here), and whether it is editable on this hardware.
enum SharingRow {
    Section(&'static str),
    Toggle {
        key: &'static str,
        label: &'static str,
        on: bool,
        note: String,
        editable: bool,
        overridden: bool,
    },
    Template {
        name: String,
        on: bool,
    },
    /// A ceiling in whole units, with the machine's own total as the top of
    /// the bar. `value == 0` means no ceiling is set.
    Ceiling {
        key: &'static str,
        label: &'static str,
        value: u64,
        total: u64,
        unit: &'static str,
        overridden: bool,
    },
    Field {
        key: &'static str,
        label: &'static str,
        shown: String,
        overridden: bool,
    },
}

impl SharingRow {
    fn selectable(&self) -> bool {
        !matches!(self, SharingRow::Section(_))
    }
}

/// MB the operator thinks in GB about. Ceilings are stored in MB (the wire
/// unit) and shown in GB (the human unit), and the conversion lives here so
/// the two cannot drift.
fn mb_to_gb(mb: u64) -> u64 {
    mb / 1024
}

fn gb_to_mb(gb: u64) -> u64 {
    gb * 1024
}

impl App {
    /// The effective value of every sharing control: the environment's
    /// baseline, with this dashboard's unsaved draft on top.
    fn sharing_rows(&self) -> Vec<SharingRow> {
        let s = &self.snap;
        let b = &s.baseline;
        let d = &self.draft;
        let backend = Backend::parse(&s.accelerator).unwrap_or(Backend::Cpu);
        let can_host = backend.hosts_container_sessions();
        let workloads = d.workloads.clone().unwrap_or_else(|| b.workloads.clone());
        let mut rows = vec![
            SharingRow::Section("what this machine lends"),
            SharingRow::Toggle {
                key: "share-inference",
                label: "GPU inference (chat & embeddings)",
                on: d.share_inference.unwrap_or(b.share_inference),
                note: if s.paused {
                    "paused right now — p resumes without changing this".into()
                } else {
                    format!("{} model(s) advertised", s.models.len())
                },
                editable: true,
                overridden: d.share_inference.is_some(),
            },
            SharingRow::Toggle {
                key: "share-cpu",
                label: "CPU & system RAM",
                on: d.share_cpu.unwrap_or(b.share_cpu),
                note: format!("{:.0} cores, {} GB", s.cpus, mb_to_gb(s.ram_total_mb)),
                editable: true,
                overridden: d.share_cpu.is_some(),
            },
            SharingRow::Toggle {
                key: "approval-mode",
                label: "Require my approval for new peers",
                on: d
                    .approval_mode
                    .clone()
                    .unwrap_or_else(|| b.approval_mode.clone())
                    == "manual",
                note: "consumers wait until approved; invitations connect directly".into(),
                editable: true,
                overridden: d.approval_mode.is_some(),
            },
            SharingRow::Section(if can_host {
                "container sessions for peers (each one is its own consent)"
            } else {
                "container sessions — this accelerator cannot pass a GPU into a container"
            }),
        ];
        if can_host {
            for t in fabric_worker::hostable_templates(backend) {
                rows.push(SharingRow::Template {
                    name: t.to_string(),
                    on: workloads.iter().any(|w| w == t),
                });
            }
        }
        rows.push(SharingRow::Section(
            "protocol v3.0 lanes — computing that is not a model",
        ));
        rows.push(SharingRow::Toggle {
            key: "functions",
            label: "Signed Wasm functions",
            on: d.functions.unwrap_or(s.functions_enabled),
            note: {
                let key = d
                    .functions_pubkey
                    .clone()
                    .unwrap_or_else(|| s.functions_pubkey.clone());
                if key.is_empty() {
                    "needs the catalog key below, or every call is refused".into()
                } else {
                    format!("{} calls served", s.jobs.functions)
                }
            },
            editable: true,
            overridden: d.functions.is_some(),
        });
        rows.push(SharingRow::Field {
            key: "functions-pubkey",
            label: "Catalog key to trust",
            shown: {
                let key = d
                    .functions_pubkey
                    .clone()
                    .unwrap_or_else(|| s.functions_pubkey.clone());
                if key.is_empty() {
                    "none — GET /v1/functions has it".into()
                } else {
                    format!("{}…", &key[..16.min(key.len())])
                }
            },
            overridden: d.functions_pubkey.is_some(),
        });
        rows.push(SharingRow::Toggle {
            key: "share-vectors",
            label: "Vector collections (peers' RAG indexes)",
            on: d.share_vectors.unwrap_or(s.vectors_enabled),
            note: format!("{} of {} MB used", s.vectors_used_mb, s.vectors_max_mb),
            editable: true,
            overridden: d.share_vectors.is_some(),
        });

        rows.push(SharingRow::Section(
            "ceilings — peer sessions never exceed these",
        ));
        rows.push(SharingRow::Ceiling {
            key: "max-cpus",
            label: "CPU threads",
            value: d
                .max_cpus
                .or(b.max_cpus)
                .map(|v| v.round() as u64)
                .unwrap_or(0),
            total: s.cpus.max(1.0) as u64,
            unit: "threads",
            overridden: d.max_cpus.is_some(),
        });
        rows.push(SharingRow::Ceiling {
            key: "max-vram-mb",
            label: "VRAM",
            value: mb_to_gb(d.max_vram_mb.or(b.max_vram_mb).unwrap_or(0)),
            total: mb_to_gb(s.vram_total_mb),
            unit: "GB",
            overridden: d.max_vram_mb.is_some(),
        });
        rows.push(SharingRow::Ceiling {
            key: "max-ram-mb",
            label: "System RAM",
            value: mb_to_gb(d.max_ram_mb.or(b.max_ram_mb).unwrap_or(0)),
            total: mb_to_gb(s.ram_total_mb),
            unit: "GB",
            overridden: d.max_ram_mb.is_some(),
        });
        rows.push(SharingRow::Ceiling {
            key: "max-disk-gb",
            label: "Disk (images & weights)",
            value: d.max_disk_gb.or(b.max_disk_gb).unwrap_or(0),
            // Disk has no advertised total the way VRAM does, so the bar is
            // scaled to a round number rather than pretending to know the
            // volume's size.
            total: 1000,
            unit: "GB",
            overridden: d.max_disk_gb.is_some(),
        });
        rows.push(SharingRow::Section("who, and through what"));
        rows.push(SharingRow::Field {
            key: "country",
            label: "Country for EU-only consumers",
            shown: {
                let c = d.country.clone().unwrap_or_else(|| b.country.clone());
                if c.is_empty() {
                    "undeclared (recorded as XX)".into()
                } else {
                    c
                }
            },
            overridden: d.country.is_some(),
        });
        rows.push(SharingRow::Field {
            key: "colibri",
            label: "Colibri gateway (frontier MoE models)",
            shown: {
                let v = d.colibri_base.clone().unwrap_or_else(|| b.colibri.clone());
                if v.is_empty() {
                    "none — lend only local models".into()
                } else {
                    v
                }
            },
            overridden: d.colibri_base.is_some(),
        });
        rows.push(SharingRow::Field {
            key: "colibri-key",
            label: "Colibri API key",
            shown: match d.colibri_api_key.as_deref() {
                Some("") => "(cleared)".into(),
                Some(_) => "(set here)".into(),
                None => "(unchanged)".into(),
            },
            overridden: d.colibri_api_key.is_some(),
        });
        rows
    }

    fn sharing_clamp(&mut self, rows: usize) {
        if self.sharing_sel >= rows {
            self.sharing_sel = rows.saturating_sub(1);
        }
    }

    /// Move the selection to the next selectable row in `step` direction.
    fn sharing_move(&mut self, step: isize) {
        let rows = self.sharing_rows();
        if rows.is_empty() {
            return;
        }
        let mut i = self.sharing_sel as isize;
        for _ in 0..rows.len() {
            i += step;
            if i < 0 {
                i = rows.len() as isize - 1;
            }
            if i >= rows.len() as isize {
                i = 0;
            }
            if rows[i as usize].selectable() {
                self.sharing_sel = i as usize;
                return;
            }
        }
    }

    fn toggle_selected(&mut self) {
        let rows = self.sharing_rows();
        let Some(row) = rows.get(self.sharing_sel) else {
            return;
        };
        match row {
            SharingRow::Toggle {
                key, on, editable, ..
            } => {
                if !editable {
                    self.say("not available on this hardware");
                    return;
                }
                let value = !on;
                match *key {
                    "share-inference" => self.draft.share_inference = Some(value),
                    "share-cpu" => self.draft.share_cpu = Some(value),
                    "approval-mode" => {
                        self.draft.approval_mode =
                            Some(if value { "manual" } else { "auto" }.to_string())
                    }
                    "functions" => self.draft.functions = Some(value),
                    "share-vectors" => self.draft.share_vectors = Some(value),
                    _ => {}
                }
            }
            SharingRow::Template { name, on } => {
                let mut list = self
                    .draft
                    .workloads
                    .clone()
                    .unwrap_or_else(|| self.snap.baseline.workloads.clone());
                if *on {
                    list.retain(|t| t != name);
                } else if !list.iter().any(|t| t == name) {
                    list.push(name.clone());
                }
                list.sort();
                self.draft.workloads = Some(list);
            }
            SharingRow::Field { .. } => self.begin_edit(),
            SharingRow::Ceiling { .. } => self.say("←/→ adjusts this ceiling, d clears it"),
            SharingRow::Section(_) => {}
        }
    }

    /// Nudge the selected ceiling. One step is one thread or one GB; with
    /// shift, a tenth of the machine, because dragging 64 GB one press at a
    /// time is not an interface.
    fn adjust_selected(&mut self, dir: i64, coarse: bool) {
        let rows = self.sharing_rows();
        let Some(SharingRow::Ceiling {
            key, value, total, ..
        }) = rows.get(self.sharing_sel)
        else {
            return;
        };
        let step = if coarse { (*total / 10).max(1) } else { 1 };
        // An unset ceiling starts from the machine's own total: the first
        // press should lower a real number, not jump from "all of it" to 1.
        let from = if *value == 0 { *total } else { *value };
        let next = (from as i64 + dir * step as i64).clamp(1, (*total).max(1) as i64) as u64;
        match *key {
            "max-cpus" => self.draft.max_cpus = Some(next as f64),
            "max-vram-mb" => self.draft.max_vram_mb = Some(gb_to_mb(next)),
            "max-ram-mb" => self.draft.max_ram_mb = Some(gb_to_mb(next)),
            "max-disk-gb" => self.draft.max_disk_gb = Some(next),
            _ => {}
        }
    }

    /// Drop the override on the selected row, handing the field back to the
    /// environment.
    fn clear_selected(&mut self) {
        let rows = self.sharing_rows();
        let key = match rows.get(self.sharing_sel) {
            Some(SharingRow::Toggle { key, .. }) => *key,
            Some(SharingRow::Ceiling { key, .. }) => *key,
            Some(SharingRow::Field { key, .. }) => *key,
            Some(SharingRow::Template { .. }) => "workloads",
            _ => return,
        };
        if let Err(e) = self.draft.clear(key) {
            self.say(e);
            return;
        }
        self.say(format!("{key}: back to the environment's value"));
    }

    fn begin_edit(&mut self) {
        let rows = self.sharing_rows();
        let Some(SharingRow::Field { key, .. }) = rows.get(self.sharing_sel) else {
            return;
        };
        let (label, masked, current) = match *key {
            "country" => (
                "Country (alpha-2, empty to declare nothing)",
                false,
                self.draft
                    .country
                    .clone()
                    .unwrap_or_else(|| self.snap.baseline.country.clone()),
            ),
            "colibri" => (
                "Colibri gateway URL (empty to switch it off)",
                false,
                self.draft
                    .colibri_base
                    .clone()
                    .unwrap_or_else(|| self.snap.baseline.colibri.clone()),
            ),
            "functions-pubkey" => (
                "Catalog key to trust: 64 hex characters, empty to trust none",
                false,
                self.draft
                    .functions_pubkey
                    .clone()
                    .unwrap_or_else(|| self.snap.functions_pubkey.clone()),
            ),
            _ => ("Colibri API key (never shown)", true, String::new()),
        };
        self.editing = Some(Editing {
            key,
            label,
            buffer: current,
            masked,
        });
    }

    /// Keys while a text field is open. Returns false once the field closes.
    fn edit_key(&mut self, key: KeyEvent) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.editing = None;
                self.say("edit cancelled");
            }
            KeyCode::Enter => {
                let (k, value) = (edit.key, edit.buffer.clone());
                self.editing = None;
                if k == "invite-label" {
                    self.mint_invitation(value);
                    return;
                }
                match self.draft.set(k, &value) {
                    Ok(()) => self.say(format!("{k} set — s applies it")),
                    Err(e) => self.say(e),
                }
            }
            KeyCode::Backspace => {
                edit.buffer.pop();
            }
            KeyCode::Char(c) => edit.buffer.push(c),
            _ => {}
        }
    }

    /// Keys that only mean something on the sharing screen. `true` when the
    /// key was consumed here.
    fn on_sharing_key(&mut self, key: KeyEvent) -> bool {
        let coarse = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.sharing_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.sharing_move(1),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_selected(),
            KeyCode::Left => self.adjust_selected(-1, coarse),
            KeyCode::Right => self.adjust_selected(1, coarse),
            KeyCode::Char('d') => self.clear_selected(),
            KeyCode::Char('s') => self.save_sharing(),
            KeyCode::Esc => {
                // Esc discards a draft here rather than quitting: leaving the
                // dashboard by accident with unsaved ceilings on screen is
                // the worse outcome.
                if self.sharing_dirty() {
                    self.discard_sharing();
                } else {
                    self.view = View::Home;
                }
            }
            _ => return false,
        }
        let rows = self.sharing_rows().len();
        self.sharing_clamp(rows);
        true
    }

    fn sharing_dirty(&self) -> bool {
        self.draft != self.saved
    }

    /// Write the draft and tell the node to re-advertise with it.
    fn save_sharing(&mut self) {
        if !self.sharing_dirty() {
            self.say("nothing to apply");
            return;
        }
        // Onto the file as it is now: the web dashboard, `kmplify-node set`
        // or the chooser may have changed other fields since this opened.
        let current = Settings::load(&self.node_dir);
        let merged = views::merge_changes(&current, &self.saved, &self.draft);
        if let Err(e) = merged.save(&self.node_dir) {
            self.say(format!("could not write settings: {e}"));
            return;
        }
        self.draft = merged.clone();
        self.saved = merged;
        // Same command in both modes: in-process it reaches the worker
        // directly, attached it goes through the control directory.
        self.send(Command::Reload);
    }

    fn discard_sharing(&mut self) {
        if self.sharing_dirty() {
            self.draft = self.saved.clone();
            self.say("changes discarded");
        }
    }
}

fn draw_sharing(f: &mut Frame, app: &App, area: Rect) {
    let [body, note] = Layout::vertical([Constraint::Min(6), Constraint::Length(4)]).areas(area);
    let rows = app.sharing_rows();
    let width = body.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len() + 4);
    for (i, r) in rows.iter().enumerate() {
        if i > 0 && matches!(r, SharingRow::Section(_)) {
            lines.push(Line::raw(""));
        }
        lines.push(sharing_line(r, i == app.sharing_sel, width));
    }
    let title = if app.sharing_dirty() {
        "sharing  ● unsaved — s applies, esc discards".to_string()
    } else {
        "sharing".to_string()
    };
    f.render_widget(
        Paragraph::new(lines).block(panel_coloured(&title, view_colour(View::Sharing))),
        body,
    );

    let help = if let Some(edit) = &app.editing {
        Line::from(vec![
            Span::styled(format!("{}: ", edit.label), Style::new().fg(MUTED)),
            Span::styled(
                if edit.masked {
                    "•".repeat(edit.buffer.chars().count())
                } else {
                    edit.buffer.clone()
                },
                Style::new().fg(Color::Yellow).bold(),
            ),
            Span::styled("_   enter saves · esc cancels", Style::new().fg(MUTED)),
        ])
    } else {
        Line::from(Span::styled(
            "↑/↓ move · space toggle · ←/→ ceiling (shift: bigger step) · enter edit · d back to environment · s apply",
            Style::new().fg(MUTED),
        ))
    };
    f.render_widget(
        Paragraph::new(vec![
            help,
            Line::from(Span::styled(
                "Applying reconnects the node so the fabric hears the new terms. Hosted sessions keep running.",
                Style::new().fg(MUTED),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(panel("about")),
        note,
    );
}

fn sharing_line(row: &SharingRow, selected: bool, width: usize) -> Line<'static> {
    let base = if selected {
        Style::new().fg(Color::Black).bg(ACCENT)
    } else {
        Style::new()
    };
    let mark = |overridden: bool| {
        if overridden {
            Span::styled(" ●", Style::new().fg(Color::Yellow))
        } else {
            Span::raw("  ")
        }
    };
    // Fixed columns: the value of every row starts in the same place, so the
    // screen reads as a form rather than as ragged prose.
    const LABEL: usize = 42;
    match row {
        SharingRow::Section(title) => Line::from(Span::styled(
            format!(" {title}"),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        SharingRow::Toggle {
            label,
            on,
            note,
            editable,
            overridden,
            ..
        } => Line::from(vec![
            Span::styled(
                format!(
                    "  [{}] {:<width$}",
                    if *on { "x" } else { " " },
                    label,
                    width = LABEL - 6
                ),
                if *editable { base } else { base.fg(MUTED) },
            ),
            mark(*overridden),
            Span::styled(format!(" {note}"), Style::new().fg(MUTED)),
        ]),
        SharingRow::Template { name, on } => Line::from(vec![Span::styled(
            format!("      [{}] {name}", if *on { "x" } else { " " }),
            base,
        )]),
        SharingRow::Ceiling {
            label,
            value,
            total,
            unit,
            overridden,
            ..
        } => {
            let shown = if *value == 0 { *total } else { *value };
            let bar_width = width.min(24);
            let filled = if *total == 0 {
                0
            } else {
                (shown as usize * bar_width / (*total).max(1) as usize).min(bar_width)
            };
            Line::from(vec![
                Span::styled(format!("  {label:<24}"), base),
                Span::styled(
                    "█".repeat(filled),
                    Style::new().fg(if *value == 0 { MUTED } else { Color::Green }),
                ),
                Span::styled("░".repeat(bar_width - filled), Style::new().fg(MUTED)),
                Span::styled(
                    if *value == 0 {
                        format!("  all of it ({total} {unit})")
                    } else {
                        format!("  {value} / {total} {unit}")
                    },
                    base,
                ),
                mark(*overridden),
            ])
        }
        SharingRow::Field {
            label,
            shown,
            overridden,
            ..
        } => Line::from(vec![
            Span::styled(format!("  {label:<LABEL$}"), base),
            Span::styled(shown.clone(), base),
            mark(*overridden),
        ]),
    }
}

// ------------------------------------------------------------------ peers

/// One line of the peers screen.
enum PeerRow {
    Section(String),
    Note(String),
    Pending(peers::Pending),
    Consumer(peers::Consumer),
    Invitation(peers::Invitation),
}

impl PeerRow {
    fn selectable(&self) -> bool {
        matches!(
            self,
            PeerRow::Pending(_) | PeerRow::Consumer(_) | PeerRow::Invitation(_)
        )
    }
}

impl App {
    fn peer_rows(&self) -> Vec<PeerRow> {
        let mut rows = Vec::new();
        let Some(p) = &self.peers else {
            rows.push(PeerRow::Section("waiting for the gateway…".into()));
            return rows;
        };
        rows.push(PeerRow::Section(format!(
            "waiting for a decision ({})",
            p.pending.len()
        )));
        if p.pending.is_empty() {
            rows.push(PeerRow::Note(
                if self.snap.approval_mode == "manual" {
                    "Nobody is waiting. New consumers appear here until you decide."
                } else {
                    "Admission is automatic, so nobody has to wait. Turn on manual approval in the sharing screen to vet consumers first."
                }
                .into(),
            ));
        }
        for x in &p.pending {
            rows.push(PeerRow::Pending(x.clone()));
        }
        rows.push(PeerRow::Section(format!(
            "consumers seen recently ({})",
            p.consumers.len()
        )));
        if p.consumers.is_empty() {
            rows.push(PeerRow::Note(
                "No consumer has used this machine recently.".into(),
            ));
        }
        for x in &p.consumers {
            rows.push(PeerRow::Consumer(x.clone()));
        }
        let live: Vec<&peers::Invitation> = p.invitations.iter().filter(|i| !i.revoked).collect();
        rows.push(PeerRow::Section(format!("invitations ({})", live.len())));
        if live.is_empty() {
            rows.push(PeerRow::Note(
                "None minted. An invitation is a contract with one consumer: they pin their inference to this machine, and it bypasses manual approval.".into(),
            ));
        }
        for x in live {
            rows.push(PeerRow::Invitation(x.clone()));
        }
        rows
    }

    fn peer_move(&mut self, step: isize) {
        let rows = self.peer_rows();
        if rows.is_empty() {
            return;
        }
        let mut i = self.peers_sel as isize;
        for _ in 0..rows.len() {
            i += step;
            if i < 0 {
                i = rows.len() as isize - 1;
            }
            if i >= rows.len() as isize {
                i = 0;
            }
            if rows[i as usize].selectable() {
                self.peers_sel = i as usize;
                return;
            }
        }
        self.peers_sel = 0;
    }

    /// The node's gateway and token, or a reason there are none.
    fn gateway_auth(&self) -> Result<(String, String), String> {
        let gateway = if self.gateway.is_empty() {
            self.snap.gateway.clone()
        } else {
            self.gateway.clone()
        };
        if gateway.is_empty() {
            return Err("no gateway known yet".into());
        }
        let creds = peers::credential(&self.creds_path)
            .ok_or_else(|| format!("no credential at {}", self.creds_path.display()))?;
        Ok((gateway, creds.token))
    }

    /// Ask the rewards companion, if the operator switched it on. Slow on
    /// purpose: a balance moves on the timescale of hours, and a dashboard
    /// that spawns a process every second to watch one is a dashboard that
    /// costs its owner the machine they are trying to rent out.
    fn fetch_rewards(&mut self) {
        let stored = Settings::load(&self.node_dir);
        let companion = kmplify_node::rewards::Companion::resolve(stored.rewards_enabled());
        if companion == kmplify_node::rewards::Companion::Off {
            self.rewards = None;
            self.last_rewards_ask = Instant::now();
            return;
        }
        self.last_rewards_ask = Instant::now();
        let (tx, dir) = (self.peer_tx.clone(), self.node_dir.clone());
        tokio::spawn(async move {
            let out = kmplify_node::rewards::ask(&companion, &dir).await;
            let _ = tx.send(BgMsg::Rewards(out));
        });
    }

    fn fetch_peers(&mut self) {
        let (gateway, token) = match self.gateway_auth() {
            Ok(v) => v,
            Err(e) => {
                self.peers_error = e;
                return;
            }
        };
        self.peers_loading = true;
        self.last_peers_fetch = Instant::now();
        let tx = self.peer_tx.clone();
        tokio::spawn(async move {
            let out = peers::fetch(&gateway, &token, GATEWAY_TIMEOUT).await;
            let _ = tx.send(BgMsg::Loaded(out));
        });
    }

    /// Approve, deny, block, or clear the standing rule for the selected
    /// consumer.
    fn decide_selected(&mut self, decision: Option<&'static str>) {
        let rows = self.peer_rows();
        let consumer = match rows.get(self.peers_sel) {
            Some(PeerRow::Pending(p)) => p.consumer.clone(),
            Some(PeerRow::Consumer(c)) => c.consumer.clone(),
            _ => {
                self.say("select a consumer first");
                return;
            }
        };
        let (gateway, token) = match self.gateway_auth() {
            Ok(v) => v,
            Err(e) => return self.say(e),
        };
        let tx = self.peer_tx.clone();
        let verb = decision.unwrap_or("cleared");
        let short = consumer.clone();
        tokio::spawn(async move {
            let out = peers::decide(&gateway, &token, &consumer, decision, GATEWAY_TIMEOUT)
                .await
                .map(|()| format!("{short} {verb}"));
            let _ = tx.send(BgMsg::Acted(out));
        });
        self.say("asking the gateway…");
    }

    fn mint_invitation(&mut self, label: String) {
        let (gateway, token) = match self.gateway_auth() {
            Ok(v) => v,
            Err(e) => return self.say(e),
        };
        let tx = self.peer_tx.clone();
        tokio::spawn(async move {
            let out = peers::invite(&gateway, &token, &label, GATEWAY_TIMEOUT)
                .await
                .map(|inv| format!("invitation {} minted", inv.invitation_id));
            let _ = tx.send(BgMsg::Acted(out));
        });
        self.say("minting…");
    }

    fn selected_invitation(&self) -> Option<peers::Invitation> {
        match self.peer_rows().get(self.peers_sel) {
            Some(PeerRow::Invitation(i)) => Some(i.clone()),
            _ => None,
        }
    }

    fn hold_selected_invitation(&mut self) {
        let Some(inv) = self.selected_invitation() else {
            self.say("select an invitation first");
            return;
        };
        let (gateway, token) = match self.gateway_auth() {
            Ok(v) => v,
            Err(e) => return self.say(e),
        };
        let paused = !inv.paused;
        let tx = self.peer_tx.clone();
        tokio::spawn(async move {
            let out = peers::set_paused(&gateway, &token, &inv, paused, GATEWAY_TIMEOUT)
                .await
                .map(|()| {
                    if paused {
                        "invitation held — pinned requests are refused until resumed".into()
                    } else {
                        "invitation resumed".to_string()
                    }
                });
            let _ = tx.send(BgMsg::Acted(out));
        });
    }

    fn revoke_invitation(&mut self, id: String) {
        let (gateway, token) = match self.gateway_auth() {
            Ok(v) => v,
            Err(e) => return self.say(e),
        };
        let tx = self.peer_tx.clone();
        tokio::spawn(async move {
            let out = peers::revoke(&gateway, &token, &id, GATEWAY_TIMEOUT)
                .await
                .map(|()| "invitation revoked".to_string());
            let _ = tx.send(BgMsg::Acted(out));
        });
    }

    /// Keys that only mean something on the peers screen.
    fn on_peers_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.peer_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.peer_move(1),
            KeyCode::Char('a') => self.decide_selected(Some("approved")),
            KeyCode::Char('n') => self.decide_selected(Some("denied")),
            KeyCode::Char('b') => self.decide_selected(Some("blocked")),
            KeyCode::Char('u') => self.decide_selected(None),
            KeyCode::Char('i') => {
                self.editing = Some(Editing {
                    key: "invite-label",
                    label: "Who is this invitation for? (a note only you see)",
                    buffer: String::new(),
                    masked: false,
                });
            }
            KeyCode::Char('h') => self.hold_selected_invitation(),
            KeyCode::Char('v') => match self.selected_invitation() {
                Some(inv) => self.confirm = Some(Confirm::Revoke(inv.invitation_id)),
                None => self.say("select an invitation first"),
            },
            KeyCode::Esc => self.view = View::Home,
            _ => return false,
        }
        true
    }

    fn on_bg_msg(&mut self, msg: BgMsg) {
        match msg {
            BgMsg::Loaded(Ok(p)) => {
                self.peers_loading = false;
                self.peers_error.clear();
                self.peers = Some(p);
                let rows = self.peer_rows();
                if rows.get(self.peers_sel).map(PeerRow::selectable) != Some(true) {
                    self.peers_sel = 0;
                    self.peer_move(1);
                }
            }
            BgMsg::Loaded(Err(e)) => {
                self.peers_loading = false;
                self.peers_error = e;
            }
            BgMsg::Acted(Ok(msg)) => {
                self.say(msg);
                // The answer is on the gateway, so re-read rather than
                // guessing what the decision did to the lists.
                self.fetch_peers();
            }
            BgMsg::Acted(Err(e)) => self.say(format!("gateway refused: {e}")),
            BgMsg::Rewards(r) => self.rewards = Some(r),
        }
    }
}

fn draw_peers(f: &mut Frame, app: &App, area: Rect) {
    let [body, note] = Layout::vertical([Constraint::Min(6), Constraint::Length(4)]).areas(area);
    let rows = app.peer_rows();
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| peer_line(r, i == app.peers_sel))
        .collect();
    let mut title = "peers".to_string();
    if app.peers_loading && app.peers.is_none() {
        title.push_str("  (loading)");
    }
    if !app.peers_error.is_empty() {
        title.push_str("  (gateway unreachable)");
    }
    f.render_widget(
        Paragraph::new(lines).block(panel_coloured(&title, view_colour(View::Peers))),
        body,
    );

    let hint = if !app.peers_error.is_empty() {
        Line::from(Span::styled(
            format!("cannot ask the gateway: {}", app.peers_error),
            Style::new().fg(Color::Red),
        ))
    } else {
        Line::from(Span::styled(
            "a approve · n deny · b block · u clear the rule · i new invitation · h hold/resume · v revoke",
            Style::new().fg(MUTED),
        ))
    };
    let mode = match app.peers.as_ref().and_then(|p| p.approval_mode.clone()) {
        Some(m) if m != app.snap.approval_mode => format!(
            "The gateway still has this node as {m}; the new mode is advertised on the next reconnect.",
        ),
        _ => "Blocking ends a consumer's access in every mode. Invitations always bypass manual approval.".into(),
    };
    f.render_widget(
        Paragraph::new(vec![
            hint,
            Line::from(Span::styled(mode, Style::new().fg(MUTED))),
        ])
        .wrap(Wrap { trim: true })
        .block(panel("about")),
        note,
    );
}

fn peer_line(row: &PeerRow, selected: bool) -> Line<'static> {
    let base = if selected {
        Style::new().fg(Color::Black).bg(ACCENT)
    } else {
        Style::new()
    };
    match row {
        PeerRow::Section(title) => Line::from(Span::styled(
            format!(" {title}"),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        PeerRow::Note(text) => {
            Line::from(Span::styled(format!("   {text}"), Style::new().fg(MUTED)))
        }
        PeerRow::Pending(p) => Line::from(vec![
            Span::styled(format!("  {:<22}", p.consumer), base),
            Span::styled(
                format!(
                    "waiting {}   asked for {}",
                    human(Duration::from_secs(p.first_seen_seconds.max(0) as u64)),
                    if p.model.is_empty() { "—" } else { &p.model }
                ),
                base.fg(Color::Yellow),
            ),
        ]),
        PeerRow::Consumer(c) => {
            let rule = c.rule.clone().unwrap_or_default();
            let (mark, colour) = match rule.as_str() {
                "blocked" => ("blocked", Color::Red),
                "denied" => ("denied", Color::Red),
                "approved" => ("approved", Color::Green),
                _ => ("", MUTED),
            };
            Line::from(vec![
                Span::styled(format!("  {:<22}", c.consumer), base),
                Span::styled(
                    format!("{:<9}", if c.active { "active" } else { "idle" }),
                    if c.active {
                        base.fg(Color::Green)
                    } else {
                        base.fg(MUTED)
                    },
                ),
                Span::styled(format!("{:<18}", c.via), base.fg(MUTED)),
                Span::styled(
                    format!(
                        "{:<10}",
                        human(Duration::from_secs(c.last_seen_seconds.max(0) as u64))
                    ),
                    base.fg(MUTED),
                ),
                Span::styled(mark.to_string(), base.fg(colour)),
            ])
        }
        PeerRow::Invitation(i) => Line::from(vec![
            Span::styled(format!("  {}", i.invitation_id), base),
            Span::styled(
                format!(
                    "  {:<20}",
                    if i.label.is_empty() {
                        "(no label)".to_string()
                    } else {
                        i.label.clone()
                    }
                ),
                base,
            ),
            Span::styled(
                if i.paused {
                    "held".to_string()
                } else if i.consumer_active {
                    format!(
                        "in use {}",
                        human(Duration::from_secs(i.connected_for_seconds.max(0) as u64))
                    )
                } else {
                    "idle".to_string()
                },
                base.fg(if i.paused {
                    Color::Yellow
                } else if i.consumer_active {
                    Color::Green
                } else {
                    MUTED
                }),
            ),
        ]),
    }
}

// --------------------------------------------------------------- activity

/// The activity monitor: what this machine is doing, second by second.
///
/// A provider is lending hardware, and the question they actually have is
/// "how much of my machine is gone right now" — which no log line answers.
/// Four measurements, each with the number, a bar, and five minutes of
/// history, plus the per-core grid that tells a pinned thread apart from a
/// busy machine.
fn draw_activity(f: &mut Frame, app: &App, area: Rect) {
    // The core grid is sized to the cores it has to draw, so the panel is
    // neither half empty on a laptop nor clipped on a 128-thread server.
    let per_row = (area.width as usize / CORE_CELL).max(1);
    let core_rows = app.snap.per_core.len().div_ceil(per_row).clamp(1, 8);
    let [row1, row2, cores, holding] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(core_rows as u16 + 2),
        Constraint::Length(3),
    ])
    .areas(area);
    let [cpu, gpu] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Min(20)]).areas(row1);
    let [ram, vram] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Min(20)]).areas(row2);

    let s = &app.snap;
    meter_panel(
        f,
        cpu,
        "CPU",
        CPU_C,
        &app.meters.cpu,
        Some(format!(
            "{:.0} of {:.0} cores lent to peers",
            s.reserved_cpus.max(0.0),
            s.cpus
        )),
        &s.cpu_model,
    );
    meter_panel(
        f,
        ram,
        "System RAM",
        RAM_C,
        &app.meters.ram,
        Some(format!(
            "{} / {} GB",
            s.ram_used_mb / 1024,
            s.ram_total_mb / 1024
        )),
        &match s.max_ram_mb {
            Some(mb) => format!("at most {} GB is offered to peers", mb / 1024),
            None => "all of it may be offered to peers".to_string(),
        },
    );
    meter_panel(
        f,
        gpu,
        &format!("GPU · {}", accel_name(s)),
        GPU_C,
        &app.meters.gpu,
        s.gpu_percent.map(|_| "busy".to_string()),
        &gpu_note(s),
    );
    meter_panel(
        f,
        vram,
        "VRAM",
        VRAM_C,
        &app.meters.vram,
        vram_used_percent(s).map(|_| {
            format!(
                "{} / {} MB",
                s.vram_used_mb.min(s.vram_total_mb),
                s.vram_total_mb
            )
        }),
        &match (vram_used_percent(s).is_some(), s.max_vram_mb) {
            (false, _) => format!(
                "{} MB on the card; usage is not reported here",
                s.vram_total_mb
            ),
            (true, Some(mb)) => format!("at most {mb} MB is offered to peers"),
            (true, None) => "all of it may be offered to peers".to_string(),
        },
    );

    draw_cores(f, app, cores);
    draw_holding(f, app, holding);
}

/// One measurement: the number, a bar, and its history.
fn meter_panel(
    f: &mut Frame,
    area: Rect,
    title: &str,
    colour: Color,
    track: &Track,
    value: Option<String>,
    note: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(colour).bold(),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let [head, graph, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let latest = track.last();
    match (&value, latest) {
        (Some(v), Some(pct)) => {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    // Drawn by hand rather than with a Gauge: the same bar
                    // glyphs as the sharing screen's ceilings, and the
                    // reading sits BESIDE the bar instead of being painted
                    // over it, where the fill inverts half the digits.
                    Span::styled(
                        "█".repeat(bar_cells(pct, head.width)),
                        Style::new().fg(load_colour(pct, colour)),
                    ),
                    Span::styled(
                        "·".repeat(BAR_WIDTH - bar_cells(pct, head.width)),
                        Style::new().fg(MUTED),
                    ),
                    Span::styled(format!(" {pct:>3}%  "), Style::new().fg(colour).bold()),
                    Span::styled(v.clone(), Style::new().fg(MUTED)),
                ])),
                head,
            );
            if track.reported {
                f.render_widget(
                    Sparkline::default()
                        .data(track.window(graph.width as usize))
                        .max(100)
                        .style(Style::new().fg(colour)),
                    graph,
                );
            }
            let mut tail = vec![Span::styled(note.to_string(), Style::new().fg(MUTED))];
            if let (Some(mean), Some(peak)) = (track.mean(), track.peak()) {
                tail.push(Span::styled(
                    format!("   5 min: avg {mean}%  peak {peak}%"),
                    Style::new().fg(MUTED),
                ));
            }
            f.render_widget(Paragraph::new(Line::from(tail)), foot);
        }
        _ => {
            // Not reported is not zero, and this arm is why: a flat line
            // along the bottom of a graph reads as an idle card, which on
            // macOS would be a lie every single time. The panel says what it
            // does know instead.
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " not reported on this platform",
                    Style::new().fg(Color::Yellow),
                ))),
                head,
            );
            f.render_widget(
                Paragraph::new(note.to_string())
                    .style(Style::new().fg(MUTED))
                    .wrap(Wrap { trim: true })
                    .alignment(Alignment::Center),
                graph,
            );
        }
    }
}

/// Cells in a meter's bar. Fixed rather than proportional so the four panels
/// line up with each other and with the sharing screen's ceilings.
const BAR_WIDTH: usize = 20;

fn bar_cells(pct: u64, width: u16) -> usize {
    let cells = BAR_WIDTH.min(width.saturating_sub(12) as usize);
    (pct as usize * cells / 100).min(BAR_WIDTH)
}

/// Width of one core's `nn ███··· 42%` cell, shared by the layout that sizes
/// the panel and the code that fills it.
const CORE_CELL: usize = 22;

/// A bar per logical CPU, htop-style.
fn draw_cores(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_coloured("cores", CPU_C);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let cores = &app.snap.per_core;
    if cores.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "waiting for the first sample…",
                Style::new().fg(MUTED),
            )),
            inner,
        );
        return;
    }
    // As many columns as fit, so 8 cores and 128 cores both look deliberate.
    let columns = ((inner.width as usize / CORE_CELL).max(1)).min(cores.len());
    let rows = cores.len().div_ceil(columns);
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans = Vec::new();
        for col in 0..columns {
            let Some(load) = cores.get(col * rows + row) else {
                continue;
            };
            let pct = load.clamp(0.0, 100.0) as u64;
            let filled = (pct as usize * 10 / 100).min(10);
            spans.push(Span::styled(
                format!("{:>3} ", col * rows + row),
                Style::new().fg(MUTED),
            ));
            spans.push(Span::styled(
                "█".repeat(filled),
                Style::new().fg(load_colour(pct, CPU_C)),
            ));
            spans.push(Span::styled(
                "·".repeat(10 - filled),
                Style::new().fg(MUTED),
            ));
            spans.push(Span::styled(
                format!(" {pct:>3}%  "),
                Style::new().fg(MUTED),
            ));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// What of this machine other people are currently holding.
fn draw_holding(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.snap;
    let block = panel_coloured("what the fabric is holding", DISK_C);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // The worker's own accounting rather than a sum of the rows: it is the
    // figure the gateway is told. Compared rather than `max`ed, because
    // `(-0.0).max(0.0)` may hand back the negative zero and "cores held -0.0"
    // reads as a bug in the accounting.
    let held = if s.reserved_cpus > 0.0 {
        s.reserved_cpus
    } else {
        0.0
    };
    let line = Line::from(vec![
        field("sessions "),
        Span::styled(
            format!("{:<4}", s.sessions.len()),
            Style::new().fg(if s.sessions.is_empty() {
                MUTED
            } else {
                Color::Green
            }),
        ),
        field("cores held "),
        Span::styled(format!("{held:<6.1}"), Style::new().fg(CPU_C)),
        field("jobs "),
        Span::styled(
            format!("{:<4}", s.jobs.active),
            Style::new().fg(if s.jobs.active > 0 {
                Color::Green
            } else {
                MUTED
            }),
        ),
        field("served "),
        // Every lane, not just inference: a node hosting only functions is
        // not an idle node.
        Span::styled(
            format!("{:<8}", s.delivered.calls()),
            Style::new().fg(ACCENT),
        ),
        field("fabric disk "),
        Span::styled(
            match s.fabric_disk_mb {
                Some(mb) => format!("{:.1} GB", mb as f64 / 1024.0),
                None => "unmeasured".into(),
            },
            Style::new().fg(DISK_C),
        ),
    ]);
    f.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), inner);
}

fn accel_name(s: &Snapshot) -> String {
    if s.accelerator.is_empty() {
        "cpu".into()
    } else {
        s.accelerator.clone()
    }
}

fn gpu_note(s: &Snapshot) -> String {
    let card = if s.gpu_name.is_empty() {
        "no card detected".to_string()
    } else {
        format!("{} · {} MB", s.gpu_name, s.vram_total_mb)
    };
    match s.accelerator.as_str() {
        "metal" => format!("{card}\nmacOS reports GPU load only to privileged tools, so this node measures what it can: VRAM held by models, and the work it answers."),
        "oneapi" => format!("{card}\nno utilization probe for this vendor yet"),
        "cpu" => "no accelerator on this machine — inference runs on the CPU above".into(),
        _ => card,
    }
}

/// Green while it is nobody's problem, amber when it is filling, red when the
/// next request will not fit — over the metric's own colour, which carries
/// the identity.
fn load_colour(pct: u64, base: Color) -> Color {
    match pct {
        90..=u64::MAX => Color::Red,
        75..=89 => Color::Yellow,
        _ => base,
    }
}

fn panel_coloured(title: &str, colour: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(colour).bold(),
        ))
}

// ------------------------------------------------- shared-model screens
//
// The Trusted node, Earnings and Settings screens, and the Overview's
// trusted-node and connection panels, render `app.model`: the value the web
// dashboard serves as /api/state. Where the web page draws a chart, these
// say it in text (the readiness ring is a bar and n/6).

fn s_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

const LEVEL_TEXT: [(&str, &str); 4] = [
    ("trusted", "Trusted"),
    ("cloud", "Cloud"),
    ("community", "Community"),
    ("unknown", "Not known yet"),
];

fn level_text(level: &str) -> String {
    LEVEL_TEXT
        .iter()
        .find(|(k, _)| *k == level)
        .map(|(_, v)| (*v).to_string())
        .unwrap_or_else(|| level.to_string())
}

fn band_colour(band: &str) -> Color {
    match band {
        "green" => Color::Green,
        "amber" => Color::Yellow,
        "red" => Color::Red,
        _ => MUTED,
    }
}

/// The readiness ring as text, the same bar `status`, the startup summary
/// and `trust::readiness_bar` print: `#` passes, `-` fails or is unreadable,
/// `+` read while the first check runs, `.` still being checked.
fn readiness_text(rd: &Value) -> String {
    let cells: String = rd["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| match s_of(i, "state") {
            "pass" | "not_applicable" => '#',
            "checked" => '+',
            "checking" => '.',
            _ => '-',
        })
        .collect();
    let total = rd["total"].as_u64().unwrap_or(0);
    if s_of(rd, "band") == "none" {
        format!(
            "[{cells}] {}/{total} read",
            rd["checked"].as_u64().unwrap_or(0)
        )
    } else {
        format!(
            "[{cells}] {}/{total} checks",
            rd["passed"].as_u64().unwrap_or(0)
        )
    }
}

fn readiness_lines(rd: &Value) -> Vec<Line<'static>> {
    let colour = band_colour(s_of(rd, "band"));
    vec![
        Line::from(vec![
            Span::styled("trust readiness  ", Style::new().fg(MUTED)),
            Span::styled(readiness_text(rd), Style::new().fg(colour).bold()),
        ]),
        Line::from(Span::styled(
            s_of(rd, "verdict").to_string(),
            Style::new().fg(Color::White),
        )),
    ]
}

/// Overview: where the node stands and how ready it is.
fn trust_panel(app: &App) -> Paragraph<'static> {
    let t = &app.model["trust"];
    let st = &t["standing"];
    let mut lines = vec![Line::from(vec![
        Span::styled(
            level_text(s_of(st, "level")),
            Style::new().fg(Color::White).bold(),
        ),
        Span::raw("  "),
        Span::styled(s_of(st, "headline").to_string(), Style::new().fg(MUTED)),
    ])];
    lines.extend(readiness_lines(&t["readiness"]));
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(panel_coloured(
            "trusted node  (8 for the checklist)",
            Color::Green,
        ))
}

/// Overview: the gateway link, as the web page's Connection card has it.
fn connection_panel(app: &App) -> Paragraph<'static> {
    let n = &app.model["node"];
    let online = s_of(n, "link") == "ONLINE";
    let state = if online {
        match n["connected_s"].as_u64() {
            Some(c) => format!("connected for {}", human(Duration::from_secs(c))),
            None => "connected".into(),
        }
    } else {
        s_of(n, "link").to_ascii_lowercase()
    };
    let mut lines = vec![
        Line::from(vec![
            field("fabric"),
            Span::raw(s_of(n, "gateway").to_string()),
        ]),
        Line::from(vec![
            field("state"),
            Span::raw(state),
            if n["paused"].as_bool() == Some(true) {
                Span::styled("  paused", Style::new().fg(Color::Yellow).bold())
            } else {
                Span::raw("")
            },
        ]),
    ];
    if !online && !s_of(n, "link_detail").is_empty() {
        lines.push(Line::from(vec![
            field("problem"),
            Span::styled(
                s_of(n, "link_detail").to_string(),
                Style::new().fg(Color::Red),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        field("running"),
        Span::raw(human(Duration::from_secs(
            n["uptime_s"].as_u64().unwrap_or(0),
        ))),
        sep(),
        field("reconnects"),
        Span::raw(format!(" {}", n["reconnects"].as_u64().unwrap_or(0))),
    ]));
    lines.push(Line::from(Span::styled(
        "p pause/resume sharing · c reconnect now · x stop the node",
        Style::new().fg(MUTED),
    )));
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(panel("connection"))
}

fn mark_for(state: &str) -> (&'static str, Color) {
    match state {
        "pass" => ("✓", Color::Green),
        "fail" => ("✕", Color::Red),
        "not_applicable" => ("–", MUTED),
        _ => ("?", Color::Red),
    }
}

fn draw_trust(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.model["trust"];
    let st = &t["standing"];
    let [top, mid, list] = Layout::vertical([
        Constraint::Length(11),
        Constraint::Length(6),
        Constraint::Min(4),
    ])
    .areas(area);
    let [stands, ready] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Min(30)]).areas(top);

    let last = match t["checked_at"].as_u64() {
        Some(at) => format!(
            "{} ago (checker {})",
            human(Duration::from_secs(
                kmplify_node::identity::now_s().saturating_sub(at)
            )),
            s_of(t, "checker_version")
        ),
        None => "running".into(),
    };
    let machine = match st["baseline_ok"].as_bool() {
        None => "check still running",
        Some(true) => "meets the baseline",
        Some(false) => "does not meet the baseline yet",
    };
    let stands_lines = vec![
        Line::from(vec![
            Span::styled(
                level_text(s_of(st, "level")),
                Style::new().fg(Color::White).bold(),
            ),
            Span::raw("  "),
            Span::raw(s_of(st, "headline").to_string()),
        ]),
        Line::from(vec![
            field("fabric"),
            Span::raw(if t["fabric_line"].is_string() {
                s_of(t, "fabric_line").to_string()
            } else {
                "not read yet".into()
            }),
        ]),
        Line::from(vec![field("machine"), Span::raw(machine)]),
        Line::from(vec![field("checked"), Span::raw(last)]),
        Line::from(Span::styled(
            if t["rechecking"].as_bool() == Some(true) {
                "checking now…"
            } else {
                "R re-check now: the signed result goes to the fabric with the next heartbeat"
            },
            Style::new().fg(MUTED),
        )),
    ];
    f.render_widget(
        Paragraph::new(stands_lines)
            .wrap(Wrap { trim: true })
            .block(panel("where this node stands")),
        stands,
    );

    let rd = &t["readiness"];
    let mut ready_lines = readiness_lines(rd);
    let items: Vec<Span> = rd["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .flat_map(|i| {
                    let (m, c) = mark_for(match s_of(i, "state") {
                        "checked" => "pass",
                        "checking" => "not_applicable",
                        other => other,
                    });
                    [
                        Span::styled(format!("{m} "), Style::new().fg(c).bold()),
                        Span::raw(format!("{}   ", s_of(i, "label"))),
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    ready_lines.push(Line::from(items));
    for (n, step) in rd["steps"].as_array().into_iter().flatten().enumerate() {
        let (word, c) = match s_of(step, "state") {
            "done" => ("done", Color::Green),
            "missing" => ("to do", Color::Red),
            "open" => ("not confirmed", Color::Yellow),
            _ => ("checking", MUTED),
        };
        ready_lines.push(Line::from(vec![
            Span::styled(format!("{}. ", n + 1), Style::new().fg(MUTED)),
            Span::raw(format!("{}  ", s_of(step, "label"))),
            Span::styled(word, Style::new().fg(c)),
        ]));
    }
    f.render_widget(
        Paragraph::new(ready_lines)
            .wrap(Wrap { trim: true })
            .block(panel("trust readiness")),
        ready,
    );

    let [missing, means] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Min(30)]).areas(mid);
    let miss: Vec<Line> = match st["missing"].as_array() {
        Some(m) if !m.is_empty() => m
            .iter()
            .map(|x| Line::raw(format!("• {}", x.as_str().unwrap_or(""))))
            .collect(),
        _ => vec![Line::styled("Nothing is missing.", Style::new().fg(MUTED))],
    };
    f.render_widget(
        Paragraph::new(miss)
            .wrap(Wrap { trim: true })
            .block(panel("missing for a trusted node")),
        missing,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::raw("Personal or confidential work runs only on Trusted nodes. Trusted needs both: the provider account passed the identity check and KMPLIFY granted Trusted, and this machine meets the security baseline."),
            Line::styled("The fabric decides the level; the node never sets it.", Style::new().fg(MUTED)),
        ])
        .wrap(Wrap { trim: true })
        .block(panel("what trusted means")),
        means,
    );

    let os = std::env::consts::OS;
    let mut lines: Vec<Line> = Vec::new();
    let checklist = t["checklist"].as_array().cloned().unwrap_or_default();
    if checklist.is_empty() {
        lines.push(Line::styled(
            "The security check is still running. On Windows the update search can take up to two minutes.",
            Style::new().fg(MUTED),
        ));
    }
    for i in &checklist {
        let state = s_of(i, "state");
        let (m, c) = mark_for(state);
        let fabric = match i["fabric"].as_str() {
            Some("pass") => "fabric: passes",
            Some(_) => "fabric: fails",
            None => "fabric: not known",
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {m} "), Style::new().fg(c).bold()),
            Span::styled(
                s_of(i, "label").to_string(),
                Style::new().fg(Color::White).bold(),
            ),
            Span::styled(format!("   {fabric}"), Style::new().fg(MUTED)),
        ]));
        lines.push(Line::raw(format!("   {}", s_of(i, "text"))));
        if state == "fail" || state == "unknown" {
            for (n, step) in i["steps"].as_array().into_iter().flatten().enumerate() {
                lines.push(Line::styled(
                    format!("     {}. {}", n + 1, step.as_str().unwrap_or("")),
                    Style::new().fg(MUTED),
                ));
            }
        }
    }
    for h in t["hints"].as_array().into_iter().flatten() {
        lines.push(Line::styled(
            format!(" note: {}", h.as_str().unwrap_or("")),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((app.selected as u16, 0))
            .block(panel_owned(format!(
                "security baseline  (how to fix it on {os}; ↑/↓ scroll)"
            ))),
        list,
    );
}

fn draw_earnings(f: &mut Frame, app: &App, area: Rect) {
    let e = &app.model["earnings"];
    if e["enabled"].as_bool() != Some(true) {
        let d = &app.model["delivered"];
        let calls = d["jobs"].as_u64().unwrap_or(0)
            + d["functions"].as_u64().unwrap_or(0)
            + d["vector_ops"].as_u64().unwrap_or(0);
        f.render_widget(
            Paragraph::new(vec![
                Line::raw("This fabric does not report KMPLIFY Credits for this node (the credit system is off here, or the node has not been answered yet)."),
                Line::raw("How earning works: kmplify.io/docs"),
                Line::from(vec![field("delivered"), Span::raw(format!("{calls} calls since start"))]),
                Line::styled("this month · in total · account and rate appear once the fabric reports credits", Style::new().fg(MUTED)),
            ])
            .wrap(Wrap { trim: true })
            .block(panel("earnings")),
            area,
        );
        return;
    }
    let [month, total, account] = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Min(30),
    ])
    .areas(area);
    let big = |v: &Value, note: &'static str| {
        vec![
            Line::styled(
                v.to_string().trim_matches('"').to_string(),
                Style::new().fg(Color::White).bold(),
            ),
            Line::styled(note, Style::new().fg(MUTED)),
        ]
    };
    f.render_widget(
        Paragraph::new(big(
            &e["earned_this_month"],
            "credits, after the platform fee",
        ))
        .block(panel("this month")),
        month,
    );
    f.render_widget(
        Paragraph::new(big(&e["earned_total"], "credits")).block(panel("in total")),
        total,
    );
    let num = |v: &Value| (!v.is_null()).then(|| v.to_string());
    let mut lines = vec![Line::from(vec![
        field("account"),
        if e["linked"].as_bool() == Some(true) {
            Span::raw("linked")
        } else {
            Span::styled(
                "NOT LINKED: earnings are held until this node is linked",
                Style::new().fg(Color::Red),
            )
        },
    ])];
    if let Some(h) = num(&e["held"]).filter(|h| h != "0") {
        lines.push(Line::from(vec![
            field("held"),
            Span::raw(format!("{h} credits")),
        ]));
    }
    lines.push(Line::from(vec![
        field("gpu class"),
        Span::raw(
            e["gpu_class"]
                .as_str()
                .map(|c| c.to_ascii_uppercase())
                .unwrap_or_else(|| "none".into()),
        ),
    ]));
    if let Some(r) = num(&e["credits_per_minute"]) {
        lines.push(Line::from(vec![
            field("rate"),
            Span::raw(format!("{r} credits per minute")),
        ]));
    }
    if let Some(p) = num(&e["platform_fee_percent"]) {
        lines.push(Line::from(vec![field("fee"), Span::raw(format!("{p} %"))]));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel("account and rate")),
        account,
    );
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::styled(
        "The same settings as the web dashboard and `kmplify-node set`. 5 opens the editor for the sharing settings (space, ←/→, enter, s applies); d there hands one back to the environment (use environment).",
        Style::new().fg(MUTED),
    )];
    for x in app.model["settings"].as_array().into_iter().flatten() {
        let value = match &x["value"] {
            Value::Null => "not set".to_string(),
            Value::String(v) if v.is_empty() => "not set".to_string(),
            Value::String(v) => v.clone(),
            other => other.to_string(),
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                s_of(x, "label").to_string(),
                Style::new().fg(Color::White).bold(),
            ),
            Span::raw("  "),
            Span::styled(value, Style::new().fg(ACCENT)),
            if x["overridden"].as_bool() == Some(true) {
                Span::styled(
                    format!("  set here, overrides {}", s_of(x, "var")),
                    Style::new().fg(Color::Yellow),
                )
            } else {
                Span::styled(format!("  ({})", s_of(x, "var")), Style::new().fg(MUTED))
            },
        ]));
        lines.push(Line::styled(
            format!("  change: {}", s_of(x, "how")),
            Style::new().fg(MUTED),
        ));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((app.selected as u16, 0))
            .block(panel("settings  (↑/↓ scroll)")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App {
            attached_to: None,
            embedded: false,
            model: Value::Null,
            engine: "http://127.0.0.1:11434".into(),
            node_dir: std::env::temp_dir(),
            draft: Settings::default(),
            saved: Settings::default(),
            sharing_sel: 1,
            editing: None,
            peers: None,
            peers_error: String::new(),
            peers_sel: 0,
            peers_loading: false,
            meters: Meters::default(),
            rewards: None,
            last_rewards_ask: Instant::now(),
            last_peers_fetch: Instant::now(),
            peer_tx: tokio::sync::mpsc::unbounded_channel().0,
            creds_path: std::env::temp_dir().join("fabric_node.json"),
            gateway: "https://gw.example".into(),
            snap: Snapshot::default(),
            view: View::Home,
            selected: 0,
            log_scroll: 0,
            confirm: None,
            notice: String::new(),
            notice_at: Instant::now(),
            help: false,
            quit: false,
        }
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    /// Everything on screen for `view`, lower case.
    fn screen(a: &mut App, view: View) -> String {
        a.view = view;
        a.selected = 0;
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 80)).unwrap();
        t.draw(|f| draw(f, a)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out.to_lowercase()
    }

    fn view_of(section: &str) -> View {
        match section {
            "overview" => View::Home,
            "trust" => View::Trust,
            "sessions" => View::Sessions,
            "earnings" => View::Earnings,
            "settings" => View::Settings,
            "logs" => View::Logs,
            other => panic!("section {other} has no terminal screen"),
        }
    }

    /// The view parity contract (`kmplify_node::views`), terminal side:
    /// every section has a screen reached by its keys, every metric is on
    /// that screen, every setting of the shared model is listed, and every
    /// action has its key in the help.
    #[test]
    fn the_terminal_view_keeps_the_view_contract() {
        let mut a = app();
        a.refresh();
        for s in views::SECTIONS {
            let want = view_of(s.id);
            for key in s.tui_keys.split(' ') {
                let mut b = app();
                b.view = View::Peers; // somewhere else first
                b.view = View::Activity;
                press(&mut b, key.chars().next().unwrap());
                assert!(
                    b.view == want || (s.id == "settings" && b.view == View::Sharing),
                    "key {key} does not open {}",
                    s.id
                );
            }
        }
        for m in views::METRICS {
            let text = screen(&mut a, view_of(m.section));
            assert!(
                text.contains(&m.label.to_lowercase()),
                "the {} screen does not show {:?}",
                m.section,
                m.label
            );
        }
        let text = screen(&mut a, View::Settings);
        for x in a.model["settings"].as_array().unwrap() {
            let label = x["label"].as_str().unwrap().to_lowercase();
            assert!(text.contains(&label), "settings screen lacks {label:?}");
        }
        let help = help_text().to_lowercase();
        for act in views::ACTIONS {
            if act.id == "view_terminal" {
                continue; // this is the terminal view
            }
            assert!(
                help.contains(&act.label.to_lowercase()),
                "help names no key for {}",
                act.id
            );
            assert!(
                help_text().contains(&format!("{}  ", act.tui_key)),
                "help lacks the {} key for {}",
                act.tui_key,
                act.id
            );
        }
    }

    #[test]
    fn the_new_keys_switch_views_and_never_quit() {
        let mut a = app();
        press(&mut a, '8');
        assert!(a.view == View::Trust);
        press(&mut a, '9');
        assert!(a.view == View::Earnings);
        press(&mut a, '0');
        assert!(a.view == View::Settings);
        // Not attached, not serving here: asks nobody, quits nothing.
        press(&mut a, 'b');
        press(&mut a, 'a');
        assert!(!a.quit);
    }

    #[test]
    fn the_views_are_reachable_by_number_and_by_letter() {
        let mut a = app();
        press(&mut a, 's');
        assert!(a.view == View::Sessions);
        press(&mut a, '3');
        assert!(a.view == View::Models);
        press(&mut a, 'l');
        assert!(a.view == View::Logs);
        press(&mut a, '1');
        assert!(a.view == View::Home);
    }

    #[test]
    fn destructive_keys_ask_first() {
        let mut a = app();
        press(&mut a, 'x');
        assert!(a.confirm.is_some(), "stopping a node must be confirmed");
        // Anything but y cancels, including a stray keypress.
        press(&mut a, 'n');
        assert!(a.confirm.is_none());
        assert!(!a.quit);
    }

    #[test]
    fn evicting_needs_a_session_to_evict() {
        let mut a = app();
        a.view = View::Sessions;
        press(&mut a, 'e');
        assert!(a.confirm.is_none());
        a.snap.sessions.push(status::Session {
            session_id: "sess-1234567890".into(),
            ..Default::default()
        });
        press(&mut a, 'e');
        match a.confirm {
            Some(Confirm::Evict(ref id)) => assert_eq!(id, "sess-1234567890"),
            _ => panic!("expected an eviction confirmation"),
        }
    }

    #[test]
    fn a_confirmation_swallows_navigation() {
        let mut a = app();
        press(&mut a, 'x');
        press(&mut a, '3');
        assert!(
            a.view == View::Home,
            "the view must not change under a modal"
        );
    }

    #[test]
    fn quitting_is_never_confirmed_away() {
        let mut a = app();
        press(&mut a, 'q');
        assert!(a.quit);
    }

    #[test]
    fn the_log_pane_follows_the_tail_until_scrolled() {
        let mut a = app();
        a.view = View::Logs;
        assert_eq!(a.log_scroll, 0);
        press(&mut a, 'k');
        assert_eq!(a.log_scroll, 1);
        press(&mut a, 'j');
        assert_eq!(a.log_scroll, 0);
        // Never scrolls past the tail into negative territory.
        press(&mut a, 'j');
        assert_eq!(a.log_scroll, 0);
    }

    #[test]
    fn a_stale_snapshot_is_never_reported_as_online() {
        let mut a = app();
        a.attached_to = Some(std::env::temp_dir());
        a.snap.link = Link::Online;
        a.snap.published_at_ms = 1; // 1970, i.e. nobody is publishing
        let (label, colour) = link_style(&a);
        assert_eq!(label, "NO NODE");
        assert_eq!(colour, Color::Red);
    }

    #[test]
    fn a_gateway_url_reduces_to_its_host() {
        assert_eq!(
            gateway_host("https://fabric.kmplify.io"),
            "fabric.kmplify.io"
        );
        assert_eq!(gateway_host("http://10.0.0.2:8080/x"), "10.0.0.2:8080");
        assert_eq!(gateway_host(""), "no gateway");
    }

    /// A machine with a CUDA card, so the template checklist is populated.
    fn cuda_app() -> App {
        let mut a = app();
        a.view = View::Sharing;
        a.snap.accelerator = "cuda".into();
        a.snap.cpus = 16.0;
        a.snap.vram_total_mb = 24576;
        a.snap.ram_total_mb = 65536;
        a.snap.baseline = status::Baseline {
            share_inference: true,
            approval_mode: "auto".into(),
            ..Default::default()
        };
        a
    }

    fn row_index(a: &App, label: &str) -> usize {
        a.sharing_rows()
            .iter()
            .position(|r| match r {
                SharingRow::Toggle { label: l, .. } => l.contains(label),
                SharingRow::Ceiling { label: l, .. } => l.contains(label),
                SharingRow::Field { label: l, .. } => l.contains(label),
                SharingRow::Template { name, .. } => name.contains(label),
                SharingRow::Section(_) => false,
            })
            .unwrap_or_else(|| panic!("no row for {label}"))
    }

    #[test]
    fn the_sharing_screen_offers_every_switch_the_desktop_panel_does() {
        let a = cuda_app();
        for label in [
            "GPU inference",
            "CPU & system RAM",
            "Require my approval",
            "CPU threads",
            "VRAM",
            "System RAM",
            "Disk",
            "Country",
            "Colibri gateway",
            "Colibri API key",
        ] {
            row_index(&a, label);
        }
        // …and the container-session templates this hardware can actually run.
        assert!(a
            .sharing_rows()
            .iter()
            .any(|r| matches!(r, SharingRow::Template { .. })));
    }

    #[test]
    fn a_machine_that_cannot_host_sessions_is_offered_none() {
        let mut a = cuda_app();
        a.snap.accelerator = "metal".into();
        assert!(
            !a.sharing_rows()
                .iter()
                .any(|r| matches!(r, SharingRow::Template { .. })),
            "macOS has no GPU passthrough into containers, so there is nothing to opt into"
        );
    }

    #[test]
    fn toggling_writes_a_draft_rather_than_the_live_node() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "CPU & system RAM");
        press(&mut a, ' ');
        assert_eq!(a.draft.share_cpu, Some(true));
        assert!(a.sharing_dirty(), "an edit is unsaved until s applies it");
        press(&mut a, ' ');
        assert_eq!(a.draft.share_cpu, Some(false));
    }

    #[test]
    fn the_approval_switch_speaks_the_gateways_vocabulary() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "Require my approval");
        press(&mut a, ' ');
        assert_eq!(a.draft.approval_mode.as_deref(), Some("manual"));
        press(&mut a, ' ');
        assert_eq!(a.draft.approval_mode.as_deref(), Some("auto"));
    }

    #[test]
    fn a_ceiling_starts_from_the_machines_own_total() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "VRAM");
        // First press must lower a real number, not jump from "all of it" to 1.
        a.on_sharing_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(a.draft.max_vram_mb, Some(gb_to_mb(23)));
        a.on_sharing_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.draft.max_vram_mb, Some(gb_to_mb(24)));
        // And never past the card.
        for _ in 0..5 {
            a.on_sharing_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        assert_eq!(a.draft.max_vram_mb, Some(gb_to_mb(24)));
    }

    #[test]
    fn a_ceiling_never_reaches_zero_by_arrow_key() {
        // Zero means "no explicit ceiling" to the worker, so walking a slider
        // down must stop at 1 rather than silently mean "all of it".
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "CPU threads");
        for _ in 0..40 {
            a.on_sharing_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        }
        assert_eq!(a.draft.max_cpus, Some(1.0));
    }

    #[test]
    fn clearing_a_row_hands_it_back_to_the_environment() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "CPU & system RAM");
        press(&mut a, ' ');
        assert!(a.draft.share_cpu.is_some());
        press(&mut a, 'd');
        assert!(a.draft.share_cpu.is_none());
    }

    #[test]
    fn templates_are_a_checklist_over_the_environments_list() {
        let mut a = cuda_app();
        a.snap.baseline.workloads = vec!["ollama".into()];
        let i = a
            .sharing_rows()
            .iter()
            .position(|r| matches!(r, SharingRow::Template { name, .. } if name == "comfyui"))
            .expect("comfyui is a CUDA template");
        a.sharing_sel = i;
        press(&mut a, ' ');
        let list = a.draft.workloads.clone().unwrap();
        assert!(list.contains(&"comfyui".to_string()));
        // The one the environment already opted into is preserved, not lost.
        assert!(list.contains(&"ollama".to_string()));
    }

    #[test]
    fn typing_into_a_field_swallows_the_global_keys() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "Country");
        press(&mut a, ' ');
        assert!(a.editing.is_some(), "enter/space opens the field");
        // "q" is a country letter here, not quit.
        press(&mut a, 'q');
        assert!(!a.quit);
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.editing.is_none());
    }

    #[test]
    fn a_field_is_validated_before_it_becomes_a_draft() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "Country");
        press(&mut a, ' ');
        for c in "DEU".chars() {
            press(&mut a, c);
        }
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(a.draft.country.is_none(), "a three-letter code is refused");
        assert!(a.notice.contains("alpha-2"));
    }

    #[test]
    fn escape_discards_a_draft_before_it_leaves_the_screen() {
        let mut a = cuda_app();
        a.sharing_sel = row_index(&a, "CPU & system RAM");
        press(&mut a, ' ');
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!a.sharing_dirty(), "the draft is dropped");
        assert!(a.view == View::Sharing, "…and the screen stays put");
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.view == View::Home, "a second escape leaves");
    }

    #[test]
    fn navigation_skips_the_section_headings() {
        let mut a = cuda_app();
        a.sharing_sel = 1;
        for _ in 0..a.sharing_rows().len() * 2 {
            a.sharing_move(1);
            assert!(
                a.sharing_rows()[a.sharing_sel].selectable(),
                "landed on a heading"
            );
        }
    }

    fn peers_app() -> App {
        let mut a = app();
        a.view = View::Peers;
        a.peers = Some(Peers {
            pending: vec![peers::Pending {
                consumer: "anon-1a2b3c4d".into(),
                first_seen_seconds: 120,
                last_seen_seconds: 2,
                model: "llama3".into(),
            }],
            consumers: vec![peers::Consumer {
                consumer: "node-9".into(),
                via: "grid selection".into(),
                active: true,
                rule: None,
                ..Default::default()
            }],
            invitations: vec![
                peers::Invitation {
                    invitation_id: "7f9b2c9e-4a1d-4e5f-9c3a-2b8d1e6f0a47".into(),
                    label: "Anna's phone".into(),
                    ..Default::default()
                },
                peers::Invitation {
                    invitation_id: "dead0000-0000-0000-0000-000000000000".into(),
                    revoked: true,
                    ..Default::default()
                },
            ],
            approval_mode: Some("manual".into()),
        });
        a
    }

    #[test]
    fn the_peers_screen_lists_who_is_waiting_who_is_using_it_and_who_was_invited() {
        let a = peers_app();
        let rows = a.peer_rows();
        assert!(rows.iter().any(|r| matches!(r, PeerRow::Pending(_))));
        assert!(rows.iter().any(|r| matches!(r, PeerRow::Consumer(_))));
        let invitations = rows
            .iter()
            .filter(|r| matches!(r, PeerRow::Invitation(_)))
            .count();
        // The revoked one is history, not a row someone can act on.
        assert_eq!(invitations, 1);
    }

    #[test]
    fn an_automatic_node_is_told_why_nobody_is_waiting() {
        let mut a = peers_app();
        a.peers.as_mut().unwrap().pending.clear();
        a.snap.approval_mode = "auto".into();
        let text = a
            .peer_rows()
            .iter()
            .filter_map(|r| match r {
                PeerRow::Note(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Admission is automatic"));
    }

    #[test]
    fn navigation_only_lands_on_rows_that_can_be_acted_on() {
        let mut a = peers_app();
        for _ in 0..a.peer_rows().len() * 2 {
            a.peer_move(1);
            assert!(a.peer_rows()[a.peers_sel].selectable());
        }
    }

    #[test]
    fn deciding_needs_a_consumer_selected() {
        let mut a = peers_app();
        // Sitting on the invitation row, an approve is a mistake, not a
        // silent no-op against whoever happens to be first in the list.
        let i = a
            .peer_rows()
            .iter()
            .position(|r| matches!(r, PeerRow::Invitation(_)))
            .unwrap();
        a.peers_sel = i;
        press(&mut a, 'a');
        assert!(a.notice.contains("select a consumer"));
    }

    #[test]
    fn revoking_an_invitation_is_confirmed_first() {
        let mut a = peers_app();
        a.peers_sel = a
            .peer_rows()
            .iter()
            .position(|r| matches!(r, PeerRow::Invitation(_)))
            .unwrap();
        press(&mut a, 'v');
        match a.confirm {
            Some(Confirm::Revoke(ref id)) => assert!(id.starts_with("7f9b2c9e")),
            _ => panic!("revoking must ask first — it cannot be undone"),
        }
        // And the question says what it costs.
        assert!(a
            .confirm
            .as_ref()
            .unwrap()
            .question()
            .contains("permanently"));
    }

    #[test]
    fn minting_an_invitation_asks_who_it_is_for() {
        let mut a = peers_app();
        press(&mut a, 'i');
        let edit = a.editing.as_ref().expect("a label field opens");
        assert_eq!(edit.key, "invite-label");
        assert!(!edit.masked);
    }

    #[test]
    fn the_peers_screen_survives_a_gateway_that_says_nothing() {
        let mut a = app();
        a.view = View::Peers;
        // No fetch has answered yet: the screen renders a waiting line rather
        // than an empty box or a panic.
        assert_eq!(a.peer_rows().len(), 1);
        a.on_bg_msg(BgMsg::Loaded(Err("connection refused".into())));
        assert_eq!(a.peers_error, "connection refused");
        assert!(!a.peers_loading);
    }

    #[test]
    fn history_keeps_the_tail_and_ignores_what_was_never_measured() {
        let mut t = Track::default();
        assert!(!t.reported, "nothing has been measured yet");
        t.push(None);
        assert!(!t.reported, "a platform that will not say must not count");
        for i in 0..(HISTORY + 10) {
            t.push(Some((i % 101) as u64));
        }
        assert!(t.reported);
        assert_eq!(t.samples.len(), HISTORY);
        assert_eq!(t.last(), Some(((HISTORY + 9) % 101) as u64));
        assert_eq!(t.window(5).len(), 5);
        assert_eq!(t.window(5).last(), t.last().as_ref().copied().as_ref());
    }

    #[test]
    fn a_percentage_needs_something_to_be_a_percentage_of() {
        assert_eq!(percent(50, 100), Some(50));
        assert_eq!(percent(3, 0), None);
        // Never over 100, whatever a vendor tool reports.
        assert_eq!(percent(120, 100), Some(100));
    }

    #[test]
    fn unified_memory_reports_no_vram_usage_rather_than_zero() {
        let mut s = Snapshot {
            accelerator: "metal".into(),
            vram_total_mb: 49152,
            ..Default::default()
        };
        assert_eq!(vram_used_percent(&s), None, "macOS cannot answer this");
        s.accelerator = "cuda".into();
        s.vram_used_mb = 24576;
        assert_eq!(vram_used_percent(&s), Some(50));
    }

    #[test]
    fn the_meters_only_graph_what_was_measured() {
        let mut m = Meters::default();
        m.sample(&Snapshot {
            accelerator: "metal".into(),
            cpu_percent: 42.0,
            vram_total_mb: 49152,
            ram_total_mb: 1024,
            ram_used_mb: 512,
            gpu_percent: None,
            ..Default::default()
        });
        assert_eq!(m.cpu.last(), Some(42));
        assert_eq!(m.ram.last(), Some(50));
        assert!(!m.gpu.reported, "Metal reports no GPU load");
        assert!(!m.vram.reported, "…and no VRAM usage either");
    }

    #[test]
    fn a_meter_turns_red_before_the_next_request_fails_to_fit() {
        assert_eq!(load_colour(10, CPU_C), CPU_C);
        assert_eq!(load_colour(80, CPU_C), Color::Yellow);
        assert_eq!(load_colour(95, CPU_C), Color::Red);
    }

    #[test]
    fn a_bar_never_overflows_its_cell() {
        assert_eq!(bar_cells(0, 80), 0);
        assert_eq!(bar_cells(100, 80), BAR_WIDTH);
        assert!(bar_cells(100, 20) <= BAR_WIDTH);
        // A narrow pane shrinks the bar rather than wrapping the line.
        assert!(bar_cells(100, 14) < BAR_WIDTH);
    }

    #[test]
    fn every_screen_has_its_own_colour() {
        let views = [
            View::Home,
            View::Sessions,
            View::Models,
            View::Logs,
            View::Sharing,
            View::Peers,
            View::Activity,
        ];
        let mut seen = Vec::new();
        for v in views {
            let c = view_colour(v);
            assert!(!seen.contains(&c), "two screens share a colour");
            seen.push(c);
        }
    }

    #[test]
    fn durations_stay_short_enough_for_one_line() {
        assert_eq!(human(Duration::from_secs(42)), "42s");
        assert_eq!(human(Duration::from_secs(600)), "10m");
        assert_eq!(human(Duration::from_secs(7_260)), "2h 1m");
        assert_eq!(human(Duration::from_secs(180_000)), "2d 2h");
    }
}
