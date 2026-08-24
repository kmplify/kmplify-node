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
use ratatui::widgets::{Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use kmplify_node::control::Command;
use kmplify_node::fabric_worker::{self, WorkerConfig};
use kmplify_node::gpu::Backend;
use kmplify_node::settings::Settings;
use kmplify_node::status::{self, Link, Snapshot};

/// Repaint rate. Fast enough that a keypress feels immediate, slow enough that
/// the dashboard is invisible in the node's own CPU figure.
const FRAME: Duration = Duration::from_millis(250);

/// How often an attached dashboard re-reads the published snapshot. The node
/// writes it every [`status::PUBLISH_INTERVAL`]; reading faster only costs
/// syscalls.
const POLL: Duration = Duration::from_millis(1000);

/// Palette, kept to the sixteen ANSI colours so the dashboard looks right on
/// whatever terminal a server happens to have, light or dark, and never paints
/// its own background.
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Sessions,
    Models,
    Logs,
    /// The desktop app's "Provide this machine's Resources" panel: what this
    /// machine lends, and how much of it.
    Sharing,
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
        }
    }
}

struct App {
    /// `None` while this process owns the worker: the snapshot is read
    /// straight out of it. `Some(dir)` when attached to another process.
    attached_to: Option<PathBuf>,
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
        let s = &self.snap;
        let mut out = String::new();
        out.push_str(&format!("kmplify-node {}\n", s.version));
        out.push_str(&format!(
            "link      : {} {}\n",
            s.link.label(),
            if s.paused { "(paused)" } else { "" }
        ));
        out.push_str(&format!("node      : {}\n", s.node_id));
        out.push_str(&format!("gateway   : {}\n", s.gateway));
        out.push_str(&format!("uptime    : {}\n", human(s.uptime())));
        out.push_str(&format!(
            "accel     : {} {} ({} MB)\n",
            s.accelerator, s.gpu_name, s.vram_total_mb
        ));
        out.push_str(&format!(
            "cpu/ram   : {} · {:.0}% · {} / {} MB\n",
            s.cpu_model, s.cpu_percent, s.ram_used_mb, s.ram_total_mb
        ));
        out.push_str(&format!("models    : {}\n", s.models.join(", ")));
        out.push_str(&format!(
            "jobs      : {} active, {} finished, {} errors, avg {} ms\n",
            s.jobs.active, s.jobs.done, s.jobs.failed, s.jobs.avg_ms
        ));
        for sess in &s.sessions {
            out.push_str(&format!(
                "session   : {} {} {} cpus={}\n",
                sess.session_id, sess.template, sess.state, sess.cpus
            ));
        }
        out.push_str("\nlog\n");
        for line in &s.logs {
            out.push_str(line);
            out.push('\n');
        }
        let path = self
            .node_dir
            .join(format!("snapshot-{}.txt", status::now_ms()));
        match std::fs::write(&path, out) {
            Ok(()) => self.say(format!("wrote {}", path.display())),
            Err(e) => self.say(format!("could not write snapshot: {e}")),
        }
    }

    fn rows(&self) -> usize {
        match self.view {
            View::Sessions => self.snap.sessions.len(),
            View::Models => self.snap.models.len(),
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
                        // Standalone, this process IS the node and leaving
                        // the dashboard already tears it down; sending the
                        // command as well would ask the node to stop itself
                        // through a channel only a connected session reads.
                        Confirm::Shutdown => {
                            if self.attached() {
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
            KeyCode::Char('r') => {
                self.refresh();
                self.say("refreshed");
            }
            KeyCode::Char('w') => self.write_snapshot(),
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

    let node = if attach_mode {
        None
    } else {
        // The dashboard owns the terminal from here on, so worker logs go to
        // the ring instead of over the frame.
        status::set_quiet(true);
        Some(crate::start_node(cfg, dir.clone()).await)
    };

    // The stored sharing choices are the starting point of the draft, so the
    // screen opens showing what this node is actually applying.
    let stored = Settings::load(&dir);
    let mut app = App {
        attached_to: attach_mode.then(|| dir.clone()),
        node_dir: dir,
        draft: stored.clone(),
        saved: stored,
        sharing_sel: 1,
        editing: None,
        snap: live.unwrap_or_default(),
        view: View::Home,
        selected: 0,
        log_scroll: 0,
        confirm: None,
        notice: if attach_mode {
            "attached to the node running here".into()
        } else {
            "started a node in this terminal".into()
        },
        notice_at: Instant::now(),
        help: false,
        quit: false,
    };

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
    let code = loop {
        if last_poll.elapsed() >= POLL {
            app.refresh();
            last_poll = Instant::now();
        }
        if terminal.draw(|f| draw(f, &app)).is_err() {
            break 1;
        }
        if app.quit {
            break 0;
        }
        tokio::select! {
            key = keys_rx.recv() => match key {
                Some(k) => app.on_key(k),
                // The reader thread died; without input this is a very
                // expensive `status` command.
                None => break 1,
            },
            _ = tokio::time::sleep(FRAME) => {}
        }
    };
    ratatui::restore();
    drop(keys_rx);
    let _ = reader.join();
    status::set_quiet(false);

    if let Some(node) = node {
        println!("[kmplify-node] stopping — tearing down hosted sessions…");
        node.shutdown().await;
        println!("[kmplify-node] stopped cleanly");
    }
    code
}

// ------------------------------------------------------------------ render

fn draw(f: &mut Frame, app: &App) {
    let [header, sub, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(header_line(app), header);
    f.render_widget(sub_line(app), sub);
    match app.view {
        View::Home => draw_home(f, app, body),
        View::Sessions => draw_sessions(f, app, body),
        View::Models => draw_models(f, app, body),
        View::Logs => draw_logs(f, app, body),
        View::Sharing => draw_sharing(f, app, body),
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
    };
    Paragraph::new(Line::from(vec![
        Span::styled(" ◆ kmplify-node", Style::new().fg(ACCENT).bold()),
        Span::styled(
            "   provider · compute fabric · inference",
            Style::new().fg(MUTED),
        ),
        Span::raw("   "),
        Span::styled(title, Style::new().fg(Color::Yellow).bold()),
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
    spans.push(Span::raw(format!("node {node}")));
    spans.push(sep());
    spans.push(Span::raw(gateway_host(&s.gateway)));
    spans.push(sep());
    spans.push(Span::raw(format!("up {}", human(s.uptime()))));
    if s.reconnects > 0 {
        spans.push(sep());
        spans.push(Span::raw(format!("{} reconnects", s.reconnects)));
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
            " {quit}   1 home  2 sessions  3 models  4 log  5 sharing   space toggle  ←/→ ceiling  d environment  s apply  ? keys"
        )
    } else {
        format!(
            " {quit}   1 home  2 sessions  3 models  4 log  5 sharing   {pause}  c reconnect  e evict  x stop node  ? keys"
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
        Constraint::Min(4),
        Constraint::Length(7),
    ])
    .areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Min(30)]).areas(top);
    draw_machine(f, app, left);
    draw_work(f, app, right);

    let [models, sessions] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Min(30)]).areas(middle);
    f.render_widget(models_table(app, 0), models);
    f.render_widget(sessions_table(app, false), sessions);
    f.render_widget(log_panel(app, logs.height.saturating_sub(2) as usize), logs);
}

/// What this machine is, and how much of it is currently spoken for.
fn draw_machine(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.snap;
    let block = panel("machine");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, vram, cpu, ram, tail] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let accel = if s.accelerator.is_empty() {
        "cpu".to_string()
    } else {
        s.accelerator.clone()
    };
    let name = if s.gpu_name.is_empty() {
        "no accelerator detected".to_string()
    } else {
        s.gpu_name.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{accel:<7}"), Style::new().fg(ACCENT).bold()),
            Span::raw(name),
        ])),
        head,
    );
    f.render_widget(vram_gauge(s), vram);
    f.render_widget(
        gauge_pct(
            "cpu ",
            s.cpu_percent as f64 / 100.0,
            &format!(
                "{:.0}%  {:.0} of {:.0} cores lent",
                s.cpu_percent, s.reserved_cpus, s.cpus
            ),
        ),
        cpu,
    );
    f.render_widget(gauge("ram ", s.ram_used_mb, s.ram_total_mb, "MB"), ram);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if s.cpu_model.is_empty() {
                String::new()
            } else {
                s.cpu_model.clone()
            },
            Style::new().fg(MUTED),
        )))
        .wrap(Wrap { trim: true }),
        tail,
    );
}

/// What the fabric is getting out of it.
fn draw_work(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.snap;
    let block = panel("sharing");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(vec![
            field("inference"),
            onoff(s.share_inference && !s.paused),
            Span::raw(format!("   {} model(s) advertised", s.models.len())),
        ]),
        Line::from(vec![
            field("cpu/ram  "),
            onoff(s.share_cpu),
            field("   sessions "),
            if s.workloads.is_empty() {
                Span::styled("off", Style::new().fg(MUTED))
            } else {
                Span::styled(s.workloads.join(","), Style::new().fg(Color::Green))
            },
        ]),
        Line::from(vec![
            field("jobs     "),
            Span::styled(
                format!("{} active", s.jobs.active),
                Style::new().fg(if s.jobs.active > 0 {
                    Color::Green
                } else {
                    MUTED
                }),
            ),
            Span::raw(format!(
                "   {} finished   {} errors   avg {} ms",
                s.jobs.done, s.jobs.failed, s.jobs.avg_ms
            )),
        ]),
        Line::from(vec![
            field("last     "),
            Span::raw(if s.jobs.last_model.is_empty() {
                "nothing yet".to_string()
            } else {
                format!("{} in {} ms", s.jobs.last_model, s.jobs.last_ms)
            }),
        ]),
        Line::from(vec![
            field("admission"),
            Span::raw(if s.approval_mode.is_empty() {
                "auto".to_string()
            } else {
                s.approval_mode.clone()
            }),
            field("   country "),
            Span::raw(if s.country.is_empty() {
                "XX (undeclared)".to_string()
            } else {
                s.country.clone()
            }),
        ]),
    ];
    if s.functions_enabled || s.vectors_enabled {
        lines.push(Line::from(vec![
            field("lanes    "),
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
    if !s.link_detail.is_empty() && s.link != Link::Online {
        lines.push(Line::from(vec![
            field("last err "),
            Span::styled(s.link_detail.clone(), Style::new().fg(Color::Red)),
        ]));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let [table, note] = Layout::vertical([Constraint::Min(4), Constraint::Length(3)]).areas(area);
    f.render_widget(sessions_table(app, true), table);
    let text = if app.snap.sessions.is_empty() {
        "No peer is running a container here. Sessions are opt-in: PROVIDER_WORKLOADS lists the templates this node accepts."
    } else {
        "↑/↓ select · e evict the selected session (its container is stopped and removed)"
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
        .map(|l| Line::from(Span::styled(l.clone(), log_style(l))))
        .collect();
    let title = if app.log_scroll > 0 {
        format!("log  (scrolled {} back, ↓ to follow)", app.log_scroll)
    } else {
        "log".to_string()
    };
    Paragraph::new(shown).block(panel_owned(title))
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
                Cell::from(s.template.clone()),
                Cell::from(state_span(&s.state)),
                Cell::from(format!("{:.1}", s.cpus)),
                Cell::from(human(age)),
            ])
            .style(style)
        })
        .collect();
    let title = format!("sessions ({})", app.snap.sessions.len());
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
    .block(panel_owned(title))
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
            let style = if app.view == View::Models && i == app.selected {
                Style::new().fg(Color::Black).bg(ACCENT)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(m.clone()),
                Cell::from(Span::styled(engine, Style::new().fg(MUTED))),
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
        .block(panel_owned(title))
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
    "1/h home   2/s sessions   3/m models   4/l log   5/g sharing\n\
     ↑/↓ or j/k   move (log: scroll back)\n\
     p  pause or resume sharing — stays connected, advertises nothing\n\
     c  reconnect to the gateway now\n\
     e  evict the selected session (sessions view)\n\
     x  stop the node   ·   w write a snapshot file   ·   r refresh\n\
     q  leave the dashboard (stops the node only if it started here)\n\
     \n\
     sharing (5): space toggles, ←/→ moves a ceiling (shift for a bigger\n\
     step), enter edits a field, d hands one back to the environment,\n\
     s applies — which reconnects so the fabric hears the new terms."
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

fn field(name: &'static str) -> Span<'static> {
    Span::styled(name, Style::new().fg(MUTED))
}

fn onoff(on: bool) -> Span<'static> {
    if on {
        Span::styled("on ", Style::new().fg(Color::Green).bold())
    } else {
        Span::styled("off", Style::new().fg(MUTED))
    }
}

fn sep() -> Span<'static> {
    Span::styled("  ·  ", Style::new().fg(MUTED))
}

/// VRAM, or an honest admission that there is no such number on this host.
///
/// Unified-memory backends (Metal, oneAPI) have no distinct "used VRAM" to
/// read, and a bar sitting at 0% looks like a measurement of an idle card
/// rather than the absence of a measurement.
fn vram_gauge(s: &Snapshot) -> Gauge<'static> {
    if !matches!(s.accelerator.as_str(), "cuda" | "rocm") {
        let text = if s.vram_total_mb == 0 {
            "vram  no accelerator".to_string()
        } else {
            format!("vram  {} MB, usage not reported", s.vram_total_mb)
        };
        return gauge_pct("vram", 0.0, &text);
    }
    gauge("vram", s.vram_used_mb, s.vram_total_mb, "MB")
}

fn gauge(label: &'static str, used: u64, total: u64, unit: &str) -> Gauge<'static> {
    let ratio = if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64).clamp(0.0, 1.0)
    };
    let text = if total == 0 {
        format!("{label}  not reported")
    } else {
        format!("{label}  {used} / {total} {unit}")
    };
    gauge_pct(label, ratio, &text)
}

fn gauge_pct(_label: &str, ratio: f64, text: &str) -> Gauge<'static> {
    // Green until it matters, amber when it is filling, red when a peer's next
    // request will not fit.
    let colour = match ratio {
        r if r >= 0.9 => Color::Red,
        r if r >= 0.7 => Color::Yellow,
        _ => Color::Green,
    };
    Gauge::default()
        .gauge_style(Style::new().fg(colour))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(Span::styled(
            text.to_string(),
            Style::new().fg(Color::White),
        ))
        .use_unicode(true)
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
        if let Err(e) = self.draft.save(&self.node_dir) {
            self.say(format!("could not write settings: {e}"));
            return;
        }
        self.saved = self.draft.clone();
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
    f.render_widget(Paragraph::new(lines).block(panel_owned(title)), body);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App {
            attached_to: None,
            node_dir: std::env::temp_dir(),
            draft: Settings::default(),
            saved: Settings::default(),
            sharing_sel: 1,
            editing: None,
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

    #[test]
    fn durations_stay_short_enough_for_one_line() {
        assert_eq!(human(Duration::from_secs(42)), "42s");
        assert_eq!(human(Duration::from_secs(600)), "10m");
        assert_eq!(human(Duration::from_secs(7_260)), "2h 1m");
        assert_eq!(human(Duration::from_secs(180_000)), "2d 2h");
    }
}
