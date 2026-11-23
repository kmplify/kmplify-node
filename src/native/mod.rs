//! The native app window: the node's views, drawn with egui.
//!
//! The `app` view (`--ui app`, `--ui native`, the App window buttons and
//! the `a` key, `kmplify-node window`). It replaces the interim browser app
//! window, which stays as the fallback where this one is not built in or
//! cannot open (a Linux box without a display, a static musl build).
//!
//! It is a process of its own, attached to the node the way an attached
//! terminal dashboard is: it reads what the node publishes (`status.json`,
//! `router.json`), builds the SAME [`crate::views::model`] the web page and
//! the terminal view render, and acts through the SAME
//! [`crate::views::perform`] (as [`crate::views::Target::Attached`]: the
//! node's `control/` directory and its settings file). So it carries the
//! parity contract (`views::{SECTIONS, METRICS, ACTIONS}`) by construction,
//! and the tests below check it against what a rendered frame shows. Being
//! its own process also gives the window toolkit the main thread it needs
//! on macOS without restructuring the node, and a window that crashes
//! takes nothing of the node with it.
//!
//! The LAN router is a section of the contract like the others: the window
//! renders `model["router"]` ([`crate::views::router_model`]) and edits the
//! router's settings and key through [`crate::views::perform`].

mod chart;
mod icon;
pub use icon::window_icon;
pub mod theme;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, Margin, RichText, Stroke};
use serde_json::{json, Value};

use crate::router::{PeerState, RouterState};
use crate::status::{Link, Snapshot};
use crate::views::{self, Act, Target};

/// The engine the node lends, handed over by the node that opens the
/// window (it may differ from this process's environment, e.g. a managed
/// vLLM).
pub const ENGINE_ENV: &str = "KMPLIFY_WINDOW_ENGINE";
/// A window that exits with a failure within this long after its start did
/// not get a display; the node falls back to the browser app window.
pub const START_GRACE: Duration = Duration::from_secs(4);
/// Exit code of `kmplify-node window` when no window can be opened here.
pub const EXIT_NO_DISPLAY: i32 = 3;

const REFRESH: Duration = Duration::from_millis(1000);
const NOTICE_TTL: Duration = Duration::from_secs(8);

/// The router's section (part of the contract since the router port's
/// phase 4; kept for callers that named it).
pub const ROUTER_SECTION: (&str, &str) = ("router", "LAN router");

/// Can a window open here at all? On Linux it needs an X11 or Wayland
/// session; macOS and Windows always have one for an interactive user.
pub fn display_available() -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        return true;
    }
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// Open the window for the node in `node_dir` and block until it closes.
/// Must be called on the process's main thread (macOS requires it).
pub fn run(node_dir: PathBuf, engine: String) -> i32 {
    if !display_available() {
        eprintln!("kmplify-node window: no display here (no DISPLAY or WAYLAND_DISPLAY)");
        return EXIT_NO_DISPLAY;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KMPLIFY Node")
            .with_app_id("io.kmplify.node")
            .with_icon(icon::window_icon(256))
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    let app = App::new(node_dir, engine);
    match eframe::run_native(
        "KMPLIFY Node",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("kmplify-node window: {e}");
            EXIT_NO_DISPLAY
        }
    }
}

/// A confirmation the window is waiting for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirm {
    StopNode,
    StopSession(String),
    NewKey,
}

/// The window's state between frames.
pub struct App {
    node_dir: PathBuf,
    engine: String,
    section: &'static str,
    model: Value,
    snap: Option<Snapshot>,
    router: Option<RouterState>,
    /// The node published recently.
    live: bool,
    /// It did at some point while this window was open.
    was_live: bool,
    last_refresh: Option<Instant>,
    notice: Option<(String, bool, Instant)>,
    /// Text being typed into a setting, by key.
    drafts: HashMap<String, String>,
    confirm: Option<Confirm>,
    log_filter: String,
    closing: bool,
    /// When a re-check was asked for here, and the checked_at of the
    /// report on screen at that moment.
    recheck_asked: Option<(Instant, Option<u64>)>,
    /// The last link code made in this window, with its instructions. It
    /// lives only in memory and is never written anywhere.
    link_text: Option<String>,
    /// The KMPLIFY mark, uploaded once.
    mark: Option<egui::TextureHandle>,
    /// The width of the content area this frame, read before the scroll
    /// area (whose inner width can grow with what was drawn last frame).
    content_width: f32,
}

/// The ids of every section the window shows, in order.
pub fn section_ids() -> Vec<&'static str> {
    views::SECTIONS.iter().map(|s| s.id).collect()
}

fn section_title(id: &str) -> &'static str {
    views::SECTIONS
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.title)
        .unwrap_or(ROUTER_SECTION.1)
}

// ------------------------------------------------------------ JSON reading

fn txt(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        other => other.to_string(),
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn gb(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

fn dur(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h {} min", s / 3600, (s % 3600) / 60),
        _ => format!("{} d {} h", s / 86_400, (s % 86_400) / 3600),
    }
}

fn ago(ms_or_s: f64, now_ms: f64) -> String {
    // The model carries epoch seconds for check times and epoch ms for
    // session starts; anything below 10^11 is seconds.
    let then_ms = if ms_or_s < 1e11 {
        ms_or_s * 1000.0
    } else {
        ms_or_s
    };
    format!("{} ago", dur((now_ms - then_ms) / 1000.0))
}

fn os_name(os: &str) -> &str {
    match os {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// The router modes as the views name them.
fn mode_text(mode: &str) -> String {
    match mode {
        "off" => "Off".into(),
        "local" => "This computer only".into(),
        "lan" => "On the network".into(),
        other => other.to_string(),
    }
}

fn level_text(level: &str) -> String {
    match level {
        "trusted" => "Trusted".into(),
        "cloud" => "Cloud".into(),
        "community" => "Community".into(),
        "unknown" => "Not known yet".into(),
        other => other.to_string(),
    }
}

// ------------------------------------------------------------- the window

impl App {
    pub fn new(node_dir: PathBuf, engine: String) -> Self {
        let mut app = Self {
            node_dir,
            engine,
            section: "overview",
            model: Value::Null,
            snap: None,
            router: None,
            live: false,
            was_live: false,
            last_refresh: None,
            notice: None,
            drafts: HashMap::new(),
            confirm: None,
            log_filter: String::new(),
            closing: false,
            recheck_asked: None,
            link_text: None,
            mark: None,
            content_width: 0.0,
        };
        app.refresh();
        app
    }

    /// Show the section `id` (one of [`section_ids`]); `false` if there is
    /// no such section.
    pub fn set_section(&mut self, id: &str) -> bool {
        match section_ids().into_iter().find(|s| *s == id) {
            Some(s) => {
                self.section = s;
                true
            }
            None => false,
        }
    }

    /// A window over a fixed model, for the tests.
    #[cfg(test)]
    fn with_model(mut model: Value, router: Option<RouterState>) -> Self {
        if router.is_some() {
            model["router"] = views::router_model(
                &views::RouterInput {
                    state: router.clone(),
                    key_fingerprint: String::new(),
                },
                &crate::settings::Settings::default(),
                "",
            );
        }
        Self {
            node_dir: std::env::temp_dir(),
            engine: String::new(),
            section: "overview",
            model,
            snap: Some(Snapshot::default()),
            router,
            live: true,
            was_live: true,
            last_refresh: Some(Instant::now()),
            notice: None,
            drafts: HashMap::new(),
            confirm: None,
            log_filter: String::new(),
            closing: false,
            recheck_asked: None,
            link_text: None,
            mark: None,
            content_width: 0.0,
        }
    }

    /// Read what the node published and rebuild the model from it.
    fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());
        let snap = crate::status::read_published(&self.node_dir);
        self.live = snap
            .as_ref()
            .is_some_and(|s| s.is_fresh() && s.link != Link::Stopped);
        if self.live {
            self.was_live = true;
        }
        let shown = snap.clone().unwrap_or_default();
        let stored = crate::settings::Settings::load(&self.node_dir);
        self.model = views::model(&views::Source {
            snap: &shown,
            stored: &stored,
            engine: &self.engine,
            logs: shown.logs.clone(),
            // The running check is in the snapshot (security_progress); the
            // model reads it from there.
            rechecking: false,
            view: shown.view.clone(),
            router: views::RouterInput::read(&self.node_dir),
        });
        self.snap = snap;
        self.router = crate::router::read_published(&self.node_dir);
        if !self.recheck_busy(Instant::now()) {
            self.recheck_asked = None;
        }
    }

    /// The node stopped while this window watched it: the window goes too.
    fn node_ended(&self) -> bool {
        self.was_live
            && match &self.snap {
                None => true,
                Some(s) => s.link == Link::Stopped,
            }
    }

    /// The checked_at of the report on screen (epoch seconds), if any.
    fn checked_at(&self) -> Option<u64> {
        self.model["trust"]["checked_at"].as_u64()
    }

    /// A re-check runs, or was asked for and has not produced a new report
    /// yet. Between the click and the node picking the command up (up to
    /// half a second, plus a publish tick) the model does not show it
    /// running; without this the button came back at once and the old
    /// report stayed on screen as if it were the answer.
    fn recheck_busy(&self, now: Instant) -> bool {
        recheck_busy(
            self.recheck_asked,
            self.model["trust"]["rechecking"].as_bool().unwrap_or(false),
            self.checked_at(),
            now,
        )
    }

    /// Carry out one action through the layer every view shares.
    fn act(&mut self, a: Act) {
        let o = views::perform(Target::Attached, &self.node_dir, &a);
        if a == Act::Recheck && o.ok {
            self.recheck_asked = Some((Instant::now(), self.checked_at()));
        }
        if matches!(a, Act::LinkCode { .. }) && o.ok {
            self.link_text = Some(o.message.clone());
            self.notice = Some((
                "link code made: valid for 10 minutes, once".into(),
                true,
                Instant::now(),
            ));
        } else {
            self.notice = Some((o.message, o.ok, Instant::now()));
        }
        if a == Act::StopNode && o.ok {
            self.closing = true;
        }
        // The node applies commands within half a second; read again soon
        // after rather than a whole refresh period later.
        self.last_refresh = Some(Instant::now() - REFRESH + Duration::from_millis(600));
    }

    /// Everything inside the window, for one frame.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let m = self.model.clone();
        let p = theme::palette(ui.ctx());
        egui::Panel::left("kn-nav")
            .resizable(false)
            .exact_size(210.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .stroke(Stroke::new(1.0, p.line))
                    .inner_margin(Margin::symmetric(14, 16)),
            )
            .show(ui, |ui| self.nav(ui, &m));
        egui::Panel::top("kn-top")
            .resizable(false)
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin {
                left: 24,
                right: 24,
                top: 16,
                bottom: 8,
            }))
            .show(ui, |ui| self.top(ui, &m));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin {
                left: 24,
                right: 24,
                top: 8,
                bottom: 16,
            }))
            .show(ui, |ui| {
                // Less the scroll bar's room.
                self.content_width = ui.available_width() - 12.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.section {
                        "overview" => self.overview(ui, &m),
                        "trust" => self.trust(ui, &m),
                        "sessions" => self.sessions(ui, &m),
                        "earnings" => self.earnings(ui, &m),
                        "settings" => self.settings(ui, &m),
                        "logs" => self.logs(ui, &m),
                        _ => self.router_section(ui, &m),
                    });
            });
        self.confirm_dialog(ui.ctx());
    }

    fn nav(&mut self, ui: &mut egui::Ui, m: &Value) {
        let p = theme::palette(ui.ctx());
        let mark = self
            .mark
            .get_or_insert_with(|| {
                ui.ctx().load_texture(
                    "kmplify-mark",
                    icon::mark_image(),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone();
        ui.horizontal(|ui| {
            // The KMPLIFY mark in the scheme's logo colour, as on the web
            // dashboard's rail.
            ui.add(
                egui::Image::new(&mark)
                    .fit_to_exact_size(egui::vec2(34.0, 34.0))
                    .tint(p.logo),
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(
                    RichText::new("KMPLIFY")
                        .family(theme::semibold())
                        .size(17.0)
                        .color(p.logo),
                );
                ui.label(RichText::new("Node").size(12.5).color(p.muted));
            });
        });
        ui.add_space(18.0);
        let failing = m["trust"]["checklist"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter(|i| i["state"] == "fail" || i["state"] == "unknown")
                    .count()
            })
            .unwrap_or(0);
        for id in section_ids() {
            let mut title = section_title(id).to_string();
            if id == "trust" && failing > 0 {
                title = format!("{title}  ({failing})");
            }
            let selected = self.section == id;
            let text = if selected {
                RichText::new(title)
                    .family(theme::semibold())
                    .color(p.accent_text)
            } else {
                RichText::new(title).color(p.ink)
            };
            let resp = ui.add_sized(
                [ui.available_width(), 30.0],
                egui::Button::new(text)
                    .fill(if selected {
                        theme::with_alpha(p.accent, 0x1c)
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(Stroke::NONE),
            );
            if resp.clicked() {
                self.section = id;
            }
        }
    }

    fn top(&mut self, ui: &mut egui::Ui, m: &Value) {
        let p = theme::palette(ui.ctx());
        let n = &m["node"];
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(section_title(self.section))
                        .family(theme::semibold())
                        .size(20.0)
                        .color(p.head),
                );
                let id = txt(&n["id_short"]);
                ui.label(
                    RichText::new(format!(
                        "v{}{} | {}",
                        txt(&m["version"]),
                        if id.is_empty() {
                            String::new()
                        } else {
                            format!(" | node {id}")
                        },
                        os_name(&txt(&m["os"]))
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.view_switch(ui, m);
                let st = &m["trust"]["standing"];
                let level = txt(&st["level"]);
                let tone = match level.as_str() {
                    "trusted" | "cloud" => p.ok,
                    "community" => p.warn,
                    _ => p.muted,
                };
                pill(ui, &level_text(&level), tone);
                let link = txt(&n["link"]);
                let paused = n["paused"].as_bool().unwrap_or(false);
                let (label, tone) = if !self.live {
                    ("Node not running".to_string(), p.bad)
                } else if link == "ONLINE" {
                    if paused {
                        ("Online, paused".to_string(), p.warn)
                    } else {
                        ("Online".to_string(), p.ok)
                    }
                } else {
                    let mut l = link.to_lowercase();
                    if let Some(f) = l.get_mut(0..1) {
                        f.make_ascii_uppercase();
                    }
                    (l, p.warn)
                };
                pill(ui, &label, tone);
            });
        });
        if !self.live {
            ui.add_space(6.0);
            banner(
                ui,
                if self.was_live {
                    "The node stopped publishing. This window shows its last state."
                } else {
                    "No node is running in this node directory. Start one: kmplify-node --ui app"
                },
                p.bad,
            );
        }
        if let Some((msg, ok, at)) = &self.notice {
            if at.elapsed() < NOTICE_TTL {
                ui.add_space(6.0);
                banner(ui, msg, if *ok { p.ok } else { p.bad });
            }
        }
    }

    /// Web / App window / Terminal: the same views the console keys open,
    /// shown only when a person started the node (a service has no views
    /// to switch).
    fn view_switch(&mut self, ui: &mut egui::Ui, m: &Value) {
        let v = &m["view"];
        if !v["switchable"].as_bool().unwrap_or(false) {
            return;
        }
        let console = v["console"].as_bool().unwrap_or(false);
        // Right to left: the last added is leftmost.
        if ui
            .add_enabled(console, egui::Button::new("Terminal"))
            .on_hover_text("bring up the terminal view in the node's console")
            .clicked()
        {
            self.act(Act::View("terminal".into()));
        }
        // This window is the app view; asking for it again only says so.
        if ui
            .add(egui::Button::new("App window").selected(true))
            .clicked()
        {
            self.act(Act::View("app".into()));
        }
        if ui.button("Web").clicked() {
            self.act(Act::View("web".into()));
        }
        ui.label(
            RichText::new("View")
                .size(12.0)
                .color(theme::palette(ui.ctx()).muted),
        );
        ui.add_space(8.0);
    }

    // --------------------------------------------------------- sections

    fn overview(&mut self, ui: &mut egui::Ui, m: &Value) {
        let wide = self.content_width > 860.0;
        let trust = |s: &mut Self, ui: &mut egui::Ui| s.overview_trust(ui, m);
        let shares = |s: &mut Self, ui: &mut egui::Ui| s.overview_shares(ui, m);
        let conn = |s: &mut Self, ui: &mut egui::Ui| s.overview_connection(ui, m);
        let machine = |s: &mut Self, ui: &mut egui::Ui| s.overview_machine(ui, m);
        if wide {
            two_columns(ui, self.content_width, |ui, col| {
                if col == 0 {
                    trust(self, ui);
                    conn(self, ui);
                } else {
                    shares(self, ui);
                    machine(self, ui);
                }
            });
        } else {
            trust(self, ui);
            shares(self, ui);
            conn(self, ui);
            machine(self, ui);
        }
    }

    fn overview_trust(&mut self, ui: &mut egui::Ui, m: &Value) {
        let st = &m["trust"]["standing"];
        let mut open = false;
        card(ui, "Trusted node", |ui| {
            standing_line(ui, st);
            if m["trust"]["readiness"].is_object() {
                readiness(ui, &m["trust"]["readiness"], true);
            }
            let missing: Vec<String> = st["missing"]
                .as_array()
                .map(|a| a.iter().map(txt).collect())
                .unwrap_or_default();
            if missing.is_empty() {
                muted(ui, "Nothing is missing.");
            } else {
                for x in missing.iter().take(4) {
                    bullet(ui, x);
                }
                if missing.len() > 4 {
                    muted(ui, &format!("and {} more", missing.len() - 4));
                }
            }
            ui.add_space(4.0);
            open = primary(ui, "Open the checklist").clicked();
        });
        if open {
            self.section = "trust";
        }
    }

    fn overview_shares(&mut self, ui: &mut egui::Ui, m: &Value) {
        let sh = &m["sharing"];
        card(ui, "What this node shares", |ui| {
            let models = sh["models"].as_array().map(Vec::len).unwrap_or(0);
            let workloads: Vec<String> = sh["workloads"]
                .as_array()
                .map(|a| a.iter().map(txt).collect())
                .unwrap_or_default();
            share_row(
                ui,
                "Chat models",
                &txt(&sh["models_text"]),
                sh["inference"].as_bool().unwrap_or(false),
                if models > 0 {
                    Some(models.to_string())
                } else {
                    None
                },
            );
            share_row(
                ui,
                "Serverless apps",
                &if workloads.is_empty() {
                    "No app templates offered.".to_string()
                } else {
                    format!(
                        "{} (each runs as a container session)",
                        workloads.join(", ")
                    )
                },
                !workloads.is_empty(),
                None,
            );
            let max_cpus = txt(&sh["max_cpus"]);
            share_row(
                ui,
                "CPU and RAM",
                &if max_cpus.is_empty() {
                    "lend spare threads and memory".to_string()
                } else {
                    format!("up to {max_cpus} threads")
                },
                sh["cpu"].as_bool().unwrap_or(false),
                None,
            );
            share_row(
                ui,
                "Functions",
                "signed Wasm functions in a sandbox",
                sh["functions"].as_bool().unwrap_or(false),
                None,
            );
            share_row(
                ui,
                "Vectors",
                "replicated vector collections",
                sh["vectors"].as_bool().unwrap_or(false),
                None,
            );
            let mut line = format!(
                "Engine: {}",
                if txt(&sh["engine"]).is_empty() {
                    "not set".into()
                } else {
                    txt(&sh["engine"])
                }
            );
            if !txt(&sh["approval_mode"]).is_empty() {
                line.push_str(&format!(" | admission: {}", txt(&sh["approval_mode"])));
            }
            if !txt(&sh["country"]).is_empty() {
                line.push_str(&format!(" | country: {}", txt(&sh["country"])));
            }
            muted(ui, &line);
        });
    }

    fn overview_connection(&mut self, ui: &mut egui::Ui, m: &Value) {
        let n = &m["node"];
        let online = n["link"] == "ONLINE";
        let paused = n["paused"].as_bool().unwrap_or(false);
        let mut chosen: Option<Act> = None;
        card(ui, "Connection", |ui| {
            let state = if online {
                match num(&n["connected_s"]) {
                    Some(s) => format!("connected for {}", dur(s)),
                    None => "connected".into(),
                }
            } else {
                txt(&n["link"]).to_lowercase()
            };
            let id = txt(&n["id_short"]);
            kv(
                ui,
                "conn",
                &[
                    ("Fabric", txt(&n["gateway"])),
                    ("State", state),
                    (
                        "Last problem",
                        if online {
                            String::new()
                        } else {
                            txt(&n["link_detail"])
                        },
                    ),
                    (
                        "Node",
                        if id.is_empty() {
                            "no identity yet".into()
                        } else {
                            format!("{id}...")
                        },
                    ),
                    ("Version", txt(&m["version"])),
                    ("Running for", dur(num(&n["uptime_s"]).unwrap_or(0.0))),
                    ("Reconnects", txt(&n["reconnects"])),
                ],
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if paused {
                    if primary(ui, "Resume sharing").clicked() {
                        chosen = Some(Act::Resume);
                    }
                } else if ui.button("Pause sharing").clicked() {
                    chosen = Some(Act::Pause);
                }
                if ui.button("Reconnect now").clicked() {
                    chosen = Some(Act::Reconnect);
                }
                if ui.button("Save a report").clicked() {
                    chosen = Some(Act::Report);
                }
                if ui.button("Stop the node").clicked() {
                    self.confirm = Some(Confirm::StopNode);
                }
            });
            if paused {
                muted(
                    ui,
                    "Paused: the connection and running sessions stay up; nothing new is offered until you resume.",
                );
            }
        });
        if let Some(a) = chosen {
            self.act(a);
        }
    }

    fn overview_machine(&mut self, ui: &mut egui::Ui, m: &Value) {
        let mc = &m["machine"];
        let p = theme::palette(ui.ctx());
        let router_meters = self
            .router
            .as_ref()
            .filter(|r| r.is_fresh())
            .map(|r| r.local_meters.clone());
        card(ui, "This machine", |ui| {
            let gpu = if txt(&mc["gpu_name"]).is_empty() {
                let a = txt(&mc["accelerator"]);
                if a.is_empty() {
                    "none detected".into()
                } else {
                    a
                }
            } else {
                let vram = num(&mc["vram_total_mb"]).unwrap_or(0.0);
                format!(
                    "{}{} ({})",
                    txt(&mc["gpu_name"]),
                    if vram > 0.0 {
                        format!(", {}", gb(vram))
                    } else {
                        String::new()
                    },
                    txt(&mc["accelerator"])
                )
            };
            let cpus = num(&mc["cpus"]).unwrap_or(0.0);
            let ram_total = num(&mc["ram_total_mb"]).unwrap_or(0.0);
            let ram_used = num(&mc["ram_used_mb"]).unwrap_or(0.0);
            kv(
                ui,
                "machine",
                &[
                    ("GPU", gpu),
                    (
                        "CPU",
                        format!(
                            "{}{}",
                            if txt(&mc["cpu_model"]).is_empty() {
                                String::new()
                            } else {
                                format!("{}, ", txt(&mc["cpu_model"]))
                            },
                            if cpus > 0.0 {
                                format!("{cpus} threads")
                            } else {
                                String::new()
                            }
                        ),
                    ),
                    (
                        "RAM",
                        if ram_total > 0.0 {
                            format!("{} of {} in use", gb(ram_used), gb(ram_total))
                        } else {
                            String::new()
                        },
                    ),
                    (
                        "Session disk",
                        num(&mc["fabric_disk_mb"])
                            .map(|d| format!("{} used by sessions", gb(d)))
                            .unwrap_or_default(),
                    ),
                ],
            );
            let cpu = num(&mc["cpu_percent"]).unwrap_or(0.0) as f32;
            ui.label(
                RichText::new(format!("CPU {cpu:.0}%"))
                    .family(theme::semibold())
                    .color(p.head),
            );
            chart::meter(ui, cpu, p.accent);
            if let Some(mt) = &router_meters {
                chart::sparkline(ui, &mt.cpu.points(), p.accent, 34.0);
            }
            if ram_total > 0.0 {
                let ram = (100.0 * ram_used / ram_total) as f32;
                ui.label(
                    RichText::new(format!("RAM {ram:.0}%"))
                        .family(theme::semibold())
                        .color(p.head),
                );
                chart::meter(ui, ram, p.accent_hover);
                if let Some(mt) = &router_meters {
                    chart::sparkline(ui, &mt.ram.points(), p.accent_hover, 34.0);
                }
            }
            match num(&mc["gpu_percent"]) {
                Some(g) => {
                    ui.label(
                        RichText::new(format!("GPU {g:.0}%"))
                            .family(theme::semibold())
                            .color(p.head),
                    );
                    chart::meter(ui, g as f32, p.ok);
                }
                None => muted(ui, "GPU load: this platform does not report it."),
            }
        });
    }

    fn trust(&mut self, ui: &mut egui::Ui, m: &Value) {
        let t = &m["trust"];
        let st = &t["standing"];
        let f = &t["fabric"];
        let rechecking = self.recheck_busy(Instant::now());
        let mut recheck = false;
        let now = num(&m["now_ms"]).unwrap_or(0.0);
        card(ui, "Where this node stands", |ui| {
            standing_line(ui, st);
            kv(
                ui,
                "stands",
                &[
                    (
                        "The fabric says",
                        if t["fabric_line"].is_null() {
                            "not read yet".into()
                        } else {
                            txt(&t["fabric_line"])
                        },
                    ),
                    ("Site", txt(&f["site"])),
                    (
                        "This machine",
                        match st["baseline_ok"].as_bool() {
                            None => "check still running".into(),
                            Some(true) => "meets the baseline".into(),
                            Some(false) => "does not meet the baseline yet".into(),
                        },
                    ),
                    (
                        "Last check",
                        match num(&t["checked_at"]) {
                            Some(at) => {
                                format!("{} (checker {})", ago(at, now), txt(&t["checker_version"]))
                            }
                            None => "running".into(),
                        },
                    ),
                    (
                        "Administrator rights",
                        match t["elevated"].as_bool() {
                            None => String::new(),
                            Some(true) => "yes".into(),
                            Some(false) => "no".into(),
                        },
                    ),
                ],
            );
            ui.add_space(4.0);
            recheck = ui
                .add_enabled(
                    !rechecking,
                    egui::Button::new(if rechecking {
                        "Checking..."
                    } else {
                        "Re-check now"
                    }),
                )
                .clicked();
            if t["readiness"].is_object() {
                readiness(ui, &t["readiness"], false);
            }
            muted(ui, "Re-check runs the self-check again and sends the signed result to the fabric with the next heartbeat. The node checks itself every 3 hours anyway; a result older than 6 hours no longer counts.");
        });
        if recheck {
            self.act(Act::Recheck);
        }
        card(ui, "Missing for a Trusted node", |ui| {
            let missing: Vec<String> = st["missing"]
                .as_array()
                .map(|a| a.iter().map(txt).collect())
                .unwrap_or_default();
            if missing.is_empty() {
                muted(ui, "Nothing is missing.");
            }
            for x in &missing {
                bullet(ui, x);
            }
        });
        card(ui, "What Trusted means", |ui| {
            ui.label(
                "Personal or confidential work runs only on Trusted nodes. Trusted needs both:",
            );
            bullet(ui, "1. Your KMPLIFY provider account passed the identity check (trader verified, 2FA on), and KMPLIFY granted Trusted for this node.");
            bullet(ui, "2. This machine meets the security baseline below. The node checks it itself and the fabric evaluates the signed result.");
            muted(ui, "The fabric decides the level; the node never sets it. What the node cannot read counts as failing.");
        });
        let p = theme::palette(ui.ctx());
        ui.label(
            RichText::new("Security baseline")
                .family(theme::semibold())
                .size(16.0)
                .color(p.head),
        );
        ui.add_space(6.0);
        let checklist = t["checklist"].as_array().cloned().unwrap_or_default();
        if checklist.is_empty() {
            card(ui, "", |ui| {
                ui.label("The security check is still running. On Windows the update search can take up to two minutes.");
            });
        }
        let os = txt(&m["os"]);
        for (n, i) in checklist.iter().enumerate() {
            let state = txt(&i["state"]);
            let bad = state == "fail" || state == "unknown";
            card(ui, "", |ui| {
                ui.horizontal_wrapped(|ui| {
                    let (mark, tone) = match state.as_str() {
                        "pass" => ("passes", p.ok),
                        "fail" => ("fails", p.bad),
                        "unknown" => ("unknown", p.bad),
                        _ => ("not checked", p.muted),
                    };
                    ui.label(
                        RichText::new(txt(&i["label"]))
                            .family(theme::semibold())
                            .color(p.head),
                    );
                    pill(ui, mark, tone);
                    match i["fabric"].as_str() {
                        None => pill(ui, "fabric: not known", p.muted),
                        Some("pass") => pill(ui, "fabric: passes", p.ok),
                        Some(_) => pill(ui, "fabric: fails", p.bad),
                    }
                });
                ui.label(theme::glyph_safe(&txt(&i["text"])));
                if !txt(&i["detail"]).is_empty() {
                    muted(ui, &txt(&i["detail"]));
                }
                let steps: Vec<String> = i["steps"]
                    .as_array()
                    .map(|a| a.iter().map(txt).collect())
                    .unwrap_or_default();
                if bad && !steps.is_empty() {
                    ui.label(
                        RichText::new(format!("How to fix it on {}", os_name(&os)))
                            .family(theme::semibold()),
                    );
                    for (k, s) in steps.iter().enumerate() {
                        bullet(ui, &format!("{}. {s}", k + 1));
                    }
                }
                if let Some(others) = i["other_os"].as_array().filter(|o| !o.is_empty()) {
                    egui::CollapsingHeader::new("Steps on other systems")
                        .id_salt(("other-os", n))
                        .show(ui, |ui| {
                            for o in others {
                                ui.label(
                                    RichText::new(os_name(&txt(&o[0]))).family(theme::semibold()),
                                );
                                for (k, s) in o[1].as_array().into_iter().flatten().enumerate() {
                                    bullet(ui, &format!("{}. {}", k + 1, txt(s)));
                                }
                            }
                        });
                }
            });
        }
        let hints: Vec<String> = t["hints"]
            .as_array()
            .map(|a| a.iter().map(txt).collect())
            .unwrap_or_default();
        if !hints.is_empty() {
            card(ui, "Notes from the check", |ui| {
                for h in &hints {
                    bullet(ui, h);
                }
            });
        }
    }

    fn sessions(&mut self, ui: &mut egui::Ui, m: &Value) {
        let se = &m["sessions"];
        let now = num(&m["now_ms"]).unwrap_or(0.0);
        let mut stop: Option<String> = None;
        card(ui, "Running now", |ui| {
            let current = se["current"].as_array().cloned().unwrap_or_default();
            if current.is_empty() {
                muted(ui, "No sessions are running on this machine.");
            } else {
                egui::Grid::new("sessions")
                    .num_columns(6)
                    .spacing([16.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for h in ["Session", "App", "State", "CPU", "Started", ""] {
                            ui.label(RichText::new(h).family(theme::semibold()));
                        }
                        ui.end_row();
                        for x in &current {
                            ui.monospace(txt(&x["id"]));
                            ui.label(txt(&x["template"]));
                            ui.label(txt(&x["state"]));
                            ui.label(match num(&x["cpus"]) {
                                Some(c) if c > 0.0 => format!("{c} threads"),
                                _ => String::new(),
                            });
                            ui.label(num(&x["since"]).map(|s| ago(s, now)).unwrap_or_default());
                            if ui.button("Stop session").clicked() {
                                stop = Some(txt(&x["session_id"]));
                            }
                            ui.end_row();
                        }
                    });
            }
            muted(
                ui,
                "Only what runs where is shown: never what a consumer sends or receives.",
            );
        });
        if let Some(id) = stop {
            self.confirm = Some(Confirm::StopSession(id));
        }
        card(ui, "Since this start", |ui| {
            let jobs = &m["jobs"];
            let failed = num(&jobs["failed"]).unwrap_or(0.0);
            kv(
                ui,
                "since",
                &[
                    ("Sessions hosted", txt(&se["hosted"])),
                    (
                        "Machine time",
                        dur(num(&se["session_seconds"]).unwrap_or(0.0)),
                    ),
                    (
                        "Chat jobs",
                        format!(
                            "{}{}",
                            txt(&jobs["done"]),
                            if failed > 0.0 {
                                format!(" ({failed} failed)")
                            } else {
                                String::new()
                            }
                        ),
                    ),
                    ("Running jobs", txt(&jobs["active"])),
                    ("Functions", txt(&m["delivered"]["functions"])),
                    ("Vector operations", txt(&m["delivered"]["vector_ops"])),
                ],
            );
        });
    }

    fn earnings(&mut self, ui: &mut egui::Ui, m: &Value) {
        let mut make_code = false;
        card(ui, "Link code", |ui| {
            muted(
                ui,
                "Links this machine to your KMPLIFY account: enter the code on the marketplace (Settings > Peer Compute) within 10 minutes. It works once.",
            );
            ui.horizontal_wrapped(|ui| {
                if ui.button("Make a link code").clicked() {
                    make_code = true;
                }
            });
            if let Some(t) = &self.link_text {
                let code = t.lines().last().unwrap_or_default().to_string();
                for l in t.lines().take(t.lines().count().saturating_sub(1)) {
                    muted(ui, l);
                }
                ui.add(egui::Label::new(RichText::new(&code).monospace()).wrap());
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(code);
                }
            }
        });
        if make_code {
            self.act(Act::LinkCode { nonce: None });
        }
        let e = &m["earnings"];
        let d = &m["delivered"];
        if !e["enabled"].as_bool().unwrap_or(false) {
            card(ui, "Earnings", |ui| {
                ui.label("This fabric does not report KMPLIFY Credits for this node (the credit system is off here, or the node has not been answered yet).");
                ui.horizontal(|ui| {
                    ui.label("How earning works:");
                    ui.hyperlink_to("kmplify.io/docs", "https://kmplify.io/docs");
                });
                let calls = num(&d["jobs"]).unwrap_or(0.0)
                    + num(&d["functions"]).unwrap_or(0.0)
                    + num(&d["vector_ops"]).unwrap_or(0.0);
                kv(
                    ui,
                    "earn0",
                    &[("Delivered since start", format!("{calls} calls"))],
                );
                muted(ui, "This month, In total and Account and rate appear once the fabric reports credits.");
            });
            return;
        }
        let p = theme::palette(ui.ctx());
        let big = |ui: &mut egui::Ui, v: String, sub: &str| {
            ui.label(
                RichText::new(v)
                    .family(theme::semibold())
                    .size(28.0)
                    .color(p.head),
            );
            muted(ui, sub);
        };
        card(ui, "This month", |ui| {
            big(
                ui,
                txt(&e["earned_this_month"]),
                "credits, after the platform fee",
            )
        });
        card(ui, "In total", |ui| {
            big(ui, txt(&e["earned_total"]), "credits")
        });
        card(ui, "Account and rate", |ui| {
            let held = num(&e["held"]).unwrap_or(0.0);
            kv(
                ui,
                "acct",
                &[
                    (
                        "Account",
                        if e["linked"].as_bool().unwrap_or(false) {
                            "linked".into()
                        } else {
                            "NOT LINKED: earnings are held until this node is linked".into()
                        },
                    ),
                    (
                        "Held",
                        if held > 0.0 {
                            format!("{held} credits")
                        } else {
                            String::new()
                        },
                    ),
                    (
                        "GPU class",
                        if txt(&e["gpu_class"]).is_empty() {
                            "none".into()
                        } else {
                            format!(
                                "{}{}",
                                txt(&e["gpu_class"]).to_uppercase(),
                                num(&e["vram_gb"])
                                    .map(|g| format!(" ({g} GB)"))
                                    .unwrap_or_default()
                            )
                        },
                    ),
                    (
                        "Rate",
                        num(&e["credits_per_minute"])
                            .map(|r| format!("{r} credits per minute"))
                            .unwrap_or_default(),
                    ),
                    (
                        "Platform fee",
                        num(&e["platform_fee_percent"])
                            .map(|f| format!("{f} %"))
                            .unwrap_or_default(),
                    ),
                ],
            );
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui, m: &Value) {
        let rows = m["settings"].as_array().cloned().unwrap_or_default();
        let mut chosen: Option<Act> = None;
        let p = theme::palette(ui.ctx());
        card(ui, "Settings", |ui| {
            muted(ui, "Changes here are the same as kmplify-node set: saved in the node directory, applied without a restart (the node reconnects to re-advertise), and they win over the environment until you reset them.");
            ui.add_space(6.0);
            for x in &rows {
                let key = txt(&x["key"]);
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.set_max_width((ui.available_width() - 260.0).max(240.0));
                        ui.label(
                            RichText::new(txt(&x["label"]))
                                .family(theme::semibold())
                                .color(p.head),
                        );
                        let shown = match &x["value"] {
                            Value::Null => "not set".to_string(),
                            Value::String(s) if s.is_empty() => "not set".to_string(),
                            v => txt(v),
                        };
                        let origin = if x["overridden"].as_bool().unwrap_or(false) {
                            "set here"
                        } else {
                            "from the environment"
                        };
                        ui.label(format!("{shown}  ({origin})"));
                        muted(ui, &txt(&x["how"]));
                    });
                    if !x["editable"].as_bool().unwrap_or(false) {
                        return;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if x["overridden"].as_bool().unwrap_or(false)
                            && ui.button("Use environment").clicked()
                        {
                            chosen = Some(Act::SettingClear { key: key.clone() });
                        }
                        match txt(&x["kind"]).as_str() {
                            "bool" => {
                                let on = x["value"].as_bool().unwrap_or(false);
                                let label = if on { "Turn off" } else { "Turn on" };
                                if ui.button(label).clicked() {
                                    chosen = Some(Act::Setting {
                                        key: key.clone(),
                                        value: (!on).to_string(),
                                    });
                                }
                            }
                            "choice" => {
                                let mut v = txt(&x["value"]);
                                let before = v.clone();
                                egui::ComboBox::from_id_salt(("choice", key.clone()))
                                    .selected_text(v.clone())
                                    .show_ui(ui, |ui| {
                                        for o in x["options"]
                                            .as_array()
                                            .into_iter()
                                            .flatten()
                                            .filter_map(Value::as_str)
                                        {
                                            ui.selectable_value(&mut v, o.to_string(), o);
                                        }
                                    });
                                if v != before {
                                    chosen = Some(Act::Setting {
                                        key: key.clone(),
                                        value: v,
                                    });
                                }
                            }
                            _ => {
                                let current = match &x["value"] {
                                    Value::Null => String::new(),
                                    v => txt(v),
                                };
                                if ui.button("Save").clicked() {
                                    let value =
                                        self.drafts.get(&key).cloned().unwrap_or(current.clone());
                                    chosen = Some(Act::Setting {
                                        key: key.clone(),
                                        value,
                                    });
                                    self.drafts.remove(&key);
                                }
                                let draft = self.drafts.entry(key.clone()).or_insert(current);
                                ui.add(egui::TextEdit::singleline(draft).desired_width(180.0));
                            }
                        }
                    });
                });
            }
        });
        if let Some(a) = chosen {
            self.act(a);
        }
    }

    fn logs(&mut self, ui: &mut egui::Ui, m: &Value) {
        let mut report = false;
        let logs: Vec<String> = m["logs"]
            .as_array()
            .map(|a| a.iter().map(txt).collect())
            .unwrap_or_default();
        let filter = self.log_filter.to_lowercase();
        card(ui, "Last log lines", |ui| {
            ui.horizontal(|ui| {
                ui.label("Filter");
                ui.add(egui::TextEdit::singleline(&mut self.log_filter).desired_width(220.0));
                report = ui.button("Save a report").clicked();
            });
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .id_salt("log-lines")
                .max_height(480.0)
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for l in logs
                        .iter()
                        .filter(|l| filter.is_empty() || l.to_lowercase().contains(&filter))
                    {
                        ui.monospace(theme::glyph_safe(l));
                    }
                });
            muted(
                ui,
                "Lines are redacted like everywhere else: no tokens, keys or prompts.",
            );
        });
        if report {
            self.act(Act::Report);
        }
    }

    fn router_section(&mut self, ui: &mut egui::Ui, m: &Value) {
        let p = theme::palette(ui.ctx());
        let r = &m["router"];
        let mode = txt(&r["mode"]);
        let asked = txt(&r["asked"]);
        let mut chosen: Option<Act> = None;
        let allow_now = r["allow"]
            .as_array()
            .map(|a| a.iter().map(txt).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let fp = txt(&r["key_fingerprint"]);
        card(ui, "This router", |ui| {
            ui.horizontal_wrapped(|ui| {
                let tone = if mode == "off" {
                    p.muted
                } else if mode == asked {
                    p.ok
                } else {
                    p.warn
                };
                pill(ui, &mode_text(&mode), tone);
                ui.label(if r["running"].as_bool().unwrap_or(false) {
                    "running"
                } else if mode == "off" {
                    "off until you choose"
                } else {
                    "not running here (the node is not running, or this build has no router)"
                });
            });
            let problem = txt(&r["problem"]);
            if !problem.is_empty() {
                ui.label(RichText::new(problem).color(p.warn));
            }
            kv(
                ui,
                "router",
                &[
                    ("Asked for", mode_text(&asked)),
                    (
                        "Listening",
                        if txt(&r["listening"]).is_empty() {
                            "no".into()
                        } else {
                            txt(&r["listening"])
                        },
                    ),
                    (
                        "Port",
                        if mode == "off" {
                            String::new()
                        } else {
                            txt(&r["port"])
                        },
                    ),
                    ("Discovery", txt(&r["discovery"])),
                    (
                        "Cluster key",
                        if fp.is_empty() {
                            "none yet (made when the router is turned on)".into()
                        } else {
                            format!("fingerprint {fp}")
                        },
                    ),
                ],
            );
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Mode").family(theme::semibold()));
                for o in r["modes"].as_array().into_iter().flatten() {
                    let o = txt(o);
                    if ui.selectable_label(asked == o, mode_text(&o)).clicked() && asked != o {
                        chosen = Some(Act::Setting {
                            key: "router".into(),
                            value: o,
                        });
                    }
                }
            });
            for (key, label, now) in [
                ("router-allow", "Allowlist", allow_now.clone()),
                ("router-name", "Name", txt(&r["name"])),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(label).family(theme::semibold()));
                    let draft = self
                        .drafts
                        .entry(key.to_string())
                        .or_insert_with(|| now.clone());
                    ui.add(egui::TextEdit::singleline(draft).desired_width(260.0));
                    if ui.button("Save").clicked() {
                        let value = self.drafts.remove(key).unwrap_or(now);
                        chosen = Some(Act::Setting {
                            key: key.into(),
                            value,
                        });
                    }
                });
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Cluster key").family(theme::semibold()));
                let draft = self.drafts.entry("router-join".into()).or_default();
                ui.add(
                    egui::TextEdit::singleline(draft)
                        .password(true)
                        .hint_text("64 hexadecimal characters")
                        .desired_width(260.0),
                );
                if ui.button("Join a cluster").clicked() {
                    let key = self.drafts.remove("router-join").unwrap_or_default();
                    chosen = Some(Act::RouterJoin(views::JoinKey(key.trim().to_string())));
                }
                if ui.button("New cluster key").clicked() {
                    self.confirm = Some(Confirm::NewKey);
                }
            });
            muted(ui, "Off until you choose. This computer only binds 127.0.0.1. On the network answers only the addresses on the allowlist (private, link-local or VPN ranges), and only requests signed with the cluster key; the key never crosses the network and is never shown here, only its fingerprint. Show it with kmplify-node router key on this computer.");
        });
        let wide = self.content_width > 860.0;
        let mut cards: Vec<NodeCard> = Vec::new();
        let local: Option<crate::router::NodeReport> =
            serde_json::from_value(r["local"].clone()).ok().flatten();
        let local_meters: crate::router::Meters =
            serde_json::from_value(r["local_meters"].clone()).unwrap_or_default();
        if let Some(me) = local {
            cards.push((
                format!("{} (this node)", me.name),
                "online".into(),
                p.ok,
                Some(me),
                local_meters,
                String::new(),
            ));
        }
        let peers: Vec<crate::router::Peer> =
            serde_json::from_value(r["peers"].clone()).unwrap_or_default();
        for peer in &peers {
            let tone = match peer.state {
                PeerState::Online => p.ok,
                PeerState::Found => p.muted,
                PeerState::Unreachable => p.warn,
                _ => p.bad,
            };
            cards.push((
                format!("{} {}", peer.name, peer.address),
                peer.state.label().into(),
                tone,
                peer.report.clone(),
                peer.meters.clone(),
                peer.detail.clone(),
            ));
        }
        card(ui, "Nodes on the network", |ui| {
            muted(
                ui,
                if !peers.is_empty() {
                    "The nodes this router found, with their hardware and the last minute of load."
                } else if mode == "lan" {
                    "No other node announced itself yet. Nodes appear here when they run the router in LAN mode with this cluster key and an allowlist that includes this machine."
                } else if mode == "local" {
                    "This computer only: other nodes are not looked for. On the network finds them."
                } else {
                    "The router is off. Turned on, it shows the kmplify-nodes on your network side by side, with their hardware and live load."
                },
            );
        });
        let draw = |ui: &mut egui::Ui, c: &NodeCard| {
            node_card(ui, c);
        };
        if wide {
            for pair in cards.chunks(2) {
                two_columns(ui, self.content_width, |ui, col| {
                    if let Some(c) = pair.get(col) {
                        draw(ui, c);
                    }
                });
            }
        } else {
            for c in &cards {
                draw(ui, c);
            }
        }
        card(ui, "Router log", |ui| {
            let lines: Vec<String> = r["log"]
                .as_array()
                .map(|a| a.iter().rev().take(20).map(txt).collect())
                .unwrap_or_default();
            if lines.is_empty() {
                muted(ui, "Nothing yet.");
            }
            for l in lines {
                ui.monospace(theme::glyph_safe(&l));
            }
        });
        if let Some(a) = chosen {
            self.act(a);
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = self.confirm.clone() else {
            return;
        };
        let (title, text) = match &c {
            Confirm::StopNode => (
                "Stop the node",
                "Stop this node? Hosted sessions are torn down first.".to_string(),
            ),
            Confirm::StopSession(id) => (
                "Stop session",
                format!(
                    "Stop session {} and remove its container?",
                    &id[..12.min(id.len())]
                ),
            ),
            Confirm::NewKey => (
                "New cluster key",
                "Make a new cluster key? Every other node must join again with it.".to_string(),
            ),
        };
        let mut answer: Option<bool> = None;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(text);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if primary(ui, title).clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
        match answer {
            Some(true) => {
                self.confirm = None;
                match c {
                    Confirm::StopNode => self.act(Act::StopNode),
                    Confirm::StopSession(id) => self.act(Act::StopSession(id)),
                    Confirm::NewKey => self.act(Act::RouterNewKey),
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_refresh.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        if self.node_ended() {
            // The node stopped (from here, another view, or Ctrl-C in its
            // console): nothing is left to show or to act on.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
        let _ = self.closing;
    }
}

/// How long a re-check asked for here may take before the button comes
/// back anyway (the Windows update search alone can take two minutes).
const RECHECK_PATIENCE: Duration = Duration::from_secs(240);

/// Is a re-check busy? Running (the node says so), or asked for and no newer
/// report has arrived yet, within [`RECHECK_PATIENCE`].
fn recheck_busy(
    asked: Option<(Instant, Option<u64>)>,
    running: bool,
    checked_at: Option<u64>,
    now: Instant,
) -> bool {
    if running {
        return true;
    }
    match asked {
        Some((at, before)) => {
            now.saturating_duration_since(at) < RECHECK_PATIENCE && checked_at == before
        }
        None => false,
    }
}

// ------------------------------------------------------------- widgets

/// Two columns of equal width, each held to its width (egui's `columns`
/// lets a wide child spill into its neighbour). `add` is called for column
/// 0, then column 1.
fn two_columns(ui: &mut egui::Ui, width: f32, mut add: impl FnMut(&mut egui::Ui, usize)) {
    let gap = 16.0;
    let w = ((width.min(ui.available_width()) - gap) / 2.0).floor();
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for col in 0..2 {
            ui.allocate_ui_with_layout(
                egui::vec2(w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(w);
                    ui.set_max_width(w);
                    add(ui, col);
                },
            );
        }
    });
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette(ui.ctx());
    // The width inside the margins and the stroke, so the card as a whole
    // is exactly as wide as the space it was given.
    let inner = (ui.available_width() - 2.0 * 16.0 - 2.0).max(100.0);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(theme::RADIUS)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(inner);
            ui.set_max_width(inner);
            // Left-aligned, never justified (ui.columns justifies, which
            // spreads the words of a wrapped line apart).
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                if !title.is_empty() {
                    ui.label(
                        RichText::new(title)
                            .family(theme::semibold())
                            .size(15.0)
                            .color(p.head),
                    );
                    ui.add_space(6.0);
                }
                add(ui);
            });
        });
    ui.add_space(12.0);
}

fn kv(ui: &mut egui::Ui, id: &str, rows: &[(&str, String)]) {
    let p = theme::palette(ui.ctx());
    // Values wrap inside the card: a long one (an error, a URL) must not
    // push the card past its column.
    let value_width = (ui.available_width() - 170.0).max(120.0);
    egui::Grid::new(("kv", id))
        .num_columns(2)
        .spacing([18.0, 6.0])
        .max_col_width(value_width)
        .show(ui, |ui| {
            for (k, v) in rows.iter().filter(|(_, v)| !v.is_empty()) {
                ui.label(RichText::new(*k).color(p.muted));
                ui.add(egui::Label::new(theme::glyph_safe(v)).wrap());
                ui.end_row();
            }
        });
}

/// A small rounded label. Painted as one widget (not a Frame) so that a
/// wrapping row moves it to the next line whole when it does not fit.
fn pill(ui: &mut egui::Ui, text: &str, tone: Color32) {
    let galley = egui::WidgetText::from(
        RichText::new(text)
            .size(12.0)
            .family(theme::semibold())
            .color(tone),
    )
    .into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    );
    let pad = egui::vec2(8.0, 2.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + 2.0 * pad, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 10.0, theme::with_alpha(tone, 0x22));
        painter.galley(rect.min + pad, galley, tone);
    }
}

fn banner(ui: &mut egui::Ui, text: &str, tone: Color32) {
    egui::Frame::new()
        .fill(theme::with_alpha(tone, 0x1e))
        .stroke(Stroke::new(1.0, theme::with_alpha(tone, 0x70)))
        .corner_radius(theme::RADIUS)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(theme::glyph_safe(text)).color(tone));
        });
}

fn muted(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette(ui.ctx());
    ui.label(
        RichText::new(theme::glyph_safe(text))
            .size(12.0)
            .color(p.muted),
    );
}

fn bullet(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label("\u{2022}");
        ui.label(theme::glyph_safe(text));
    });
}

fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = theme::palette(ui.ctx());
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE))
            .fill(p.accent)
            .stroke(Stroke::new(1.0, p.accent)),
    )
}

fn share_row(ui: &mut egui::Ui, title: &str, sub: &str, on: bool, count: Option<String>) {
    let p = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 90.0).max(160.0));
            ui.label(RichText::new(title).family(theme::semibold()).color(p.head));
            muted(ui, sub);
        });
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| match count {
                Some(c) => pill(ui, &c, p.ok),
                None if on => pill(ui, "on", p.ok),
                None => pill(ui, "off", p.muted),
            },
        );
    });
    ui.add_space(4.0);
}

fn standing_line(ui: &mut egui::Ui, st: &Value) {
    let p = theme::palette(ui.ctx());
    let level = txt(&st["level"]);
    let tone = match level.as_str() {
        "trusted" | "cloud" => p.ok,
        "community" => p.warn,
        _ => p.muted,
    };
    ui.horizontal_wrapped(|ui| {
        pill(ui, &level_text(&level), tone);
        ui.label(theme::glyph_safe(&txt(&st["headline"])));
    });
}

/// "Trust readiness": the web page's ring, here a bar with n/total.
fn readiness(ui: &mut egui::Ui, rd: &Value, compact: bool) {
    let p = theme::palette(ui.ctx());
    let passed = num(&rd["passed"]).unwrap_or(0.0);
    let total = num(&rd["total"]).unwrap_or(0.0).max(1.0);
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!("Trust readiness {passed:.0}/{total:.0}"))
            .family(theme::semibold())
            .color(p.head),
    );
    let tone = if passed >= total { p.ok } else { p.warn };
    chart::meter(ui, (100.0 * passed / total) as f32, tone);
    ui.label(theme::glyph_safe(&txt(&rd["verdict"])));
    if rd["checking"].as_bool().unwrap_or(false) {
        let checked = num(&rd["checked"]).unwrap_or(0.0);
        muted(ui, &format!("{checked:.0} of {total:.0} checks read"));
    }
    if let Some(items) = rd["items"].as_array() {
        ui.horizontal_wrapped(|ui| {
            for i in items {
                let tone = match i["state"].as_str() {
                    Some("pass") => p.ok,
                    Some("fail") | Some("unknown") => p.bad,
                    _ => p.muted,
                };
                pill(ui, &txt(&i["label"]), tone);
            }
        });
    }
    if let Some(steps) = rd["steps"].as_array() {
        for (n, st) in steps.iter().enumerate() {
            let state = txt(&st["state"]);
            let (word, tone) = match state.as_str() {
                "done" => ("done", p.ok),
                "missing" => ("missing", p.bad),
                _ => ("open", p.warn),
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{}.", n + 1)).color(p.muted));
                ui.label(RichText::new(txt(&st["label"])).family(theme::semibold()));
                pill(ui, word, tone);
            });
            if !compact && !txt(&st["text"]).is_empty() {
                muted(ui, &txt(&st["text"]));
            }
        }
    }
    ui.add_space(4.0);
}

type NodeCard = (
    String,
    String,
    Color32,
    Option<crate::router::NodeReport>,
    crate::router::Meters,
    String,
);

fn node_card(ui: &mut egui::Ui, c: &NodeCard) {
    let p = theme::palette(ui.ctx());
    let inner = (ui.available_width() - 2.0 * 16.0 - 2.0).max(100.0);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(theme::RADIUS)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(inner);
            ui.set_max_width(inner);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                node_card_body(ui, c)
            });
        });
    ui.add_space(12.0);
}

fn node_card_body(ui: &mut egui::Ui, c: &NodeCard) {
    let p = theme::palette(ui.ctx());
    let (title, state, tone, report, meters, detail) = c;
    {
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).family(theme::semibold()).color(p.head));
            pill(ui, state, *tone);
        });
        if !detail.is_empty() {
            muted(ui, detail);
        }
        let Some(r) = report else {
            muted(ui, "No report yet.");
            return;
        };
        let gpu = if r.gpu_name.is_empty() {
            r.accelerator.clone()
        } else {
            format!("{} ({})", r.gpu_name, gb(r.vram_total_mb as f64))
        };
        kv(
            ui,
            &format!("node-{}", r.id),
            &[
                ("GPU", gpu),
                ("CPU", format!("{} threads, {:.0}%", r.cpus, r.cpu_percent)),
                (
                    "RAM",
                    format!(
                        "{} of {}",
                        gb(r.ram_used_mb as f64),
                        gb(r.ram_total_mb as f64)
                    ),
                ),
                (
                    "Fabric",
                    format!(
                        "{}{}",
                        r.link.to_lowercase(),
                        if r.paused { ", paused" } else { "" }
                    ),
                ),
                ("Models", r.models.join(", ")),
                ("Version", r.version.clone()),
            ],
        );
        ui.label(RichText::new("CPU").size(12.0).color(p.muted));
        chart::sparkline(ui, &meters.cpu.points(), p.accent, 30.0);
        ui.label(RichText::new("RAM").size(12.0).color(p.muted));
        chart::sparkline(ui, &meters.ram.points(), p.accent_hover, 30.0);
        if meters.gpu.latest().is_some() {
            ui.label(RichText::new("GPU").size(12.0).color(p.muted));
            chart::sparkline(ui, &meters.gpu.points(), p.ok, 30.0);
        }
    }
}

/// The model a window shows before any node published, for tests and for
/// the first frame.
pub fn empty_model() -> Value {
    let snap = Snapshot::default();
    let stored = crate::settings::Settings::default();
    let mut m = views::model(&views::Source {
        snap: &snap,
        stored: &stored,
        engine: "",
        logs: Vec::new(),
        rechecking: false,
        view: crate::control::ViewState::default(),
        router: views::RouterInput::default(),
    });
    m["view"] = json!(crate::control::ViewState::default());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render one frame of `section` headless and return every piece of
    /// text it drew.
    fn render(app: &mut App, section: &'static str, width: f32) -> Vec<String> {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        app.section = section;
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 4000.0),
            )),
            ..Default::default()
        };
        // Two passes: egui sizes some containers on the first.
        let mut first = ctx.run_ui(input(), |ui| app.draw(ui));
        first.textures_delta.clear();
        let mut out = ctx.run_ui(input(), |ui| app.draw(ui));
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for clipped in &out.shapes {
            collect(&clipped.shape, &mut texts);
        }
        texts
    }

    fn collect(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
            _ => {}
        }
    }

    fn rich_model() -> Value {
        let mut m = empty_model();
        m["view"] =
            json!({"switchable": true, "console": true, "terminal_up": false, "start": "app"});
        m["node"]["link"] = json!("ONLINE");
        m["node"]["paused"] = json!(false);
        m["sessions"]["current"] = json!([{
            "id": "sess-0123456", "session_id": "sess-0123456789", "template": "ollama-cpu",
            "state": "running", "cpus": 2, "since": 1_700_000_000_000u64
        }]);
        m["earnings"] = json!({
            "enabled": true, "earned_this_month": 12, "earned_total": 40, "linked": true,
            "held": 0, "gpu_class": "b", "vram_gb": 24, "credits_per_minute": 1.5,
            "platform_fee_percent": 15
        });
        m["logs"] = json!(["12:00:00 node identity 01234567...", "12:00:01 joined"]);
        // One setting set here, so "Use environment" is offered.
        for r in m["settings"].as_array_mut().unwrap() {
            if r["key"] == "country" {
                r["overridden"] = json!(true);
            }
        }
        m
    }

    /// The parity contract, native side: every section of the contract is a
    /// section of the window, and the frame drawn for it shows every metric
    /// the contract names for it.
    #[test]
    fn the_native_window_keeps_the_view_contract() {
        let ids = section_ids();
        for s in views::SECTIONS {
            assert!(ids.contains(&s.id), "section {} missing", s.id);
            assert_eq!(section_title(s.id), s.title);
        }
        let mut app = App::with_model(rich_model(), None);
        for s in views::SECTIONS {
            let texts = render(&mut app, s.id, 1280.0);
            let all = texts.join("\n").to_lowercase();
            assert!(
                texts.iter().any(|t| t == s.title),
                "the {} section does not show its title",
                s.id
            );
            for m in views::METRICS.iter().filter(|m| m.section == s.id) {
                assert!(
                    all.contains(&m.label.to_lowercase()),
                    "the {} section of the window does not show {:?}; it drew:\n{}",
                    s.id,
                    m.label,
                    texts.join(" | ")
                );
            }
        }
    }

    /// Every action of the contract has a control in the window, labelled as
    /// the contract names it, and each control acts through views::perform
    /// with the matching [`Act`].
    #[test]
    fn every_action_has_a_control() {
        const SRC: &str = include_str!("mod.rs");
        let mut app = App::with_model(rich_model(), None);
        let mut drawn: Vec<String> = Vec::new();
        for id in section_ids() {
            drawn.extend(render(&mut app, id, 1280.0));
        }
        // The paused state shows Resume instead of Pause.
        let mut paused = rich_model();
        paused["node"]["paused"] = json!(true);
        let mut app = App::with_model(paused, None);
        drawn.extend(render(&mut app, "overview", 1280.0));
        for a in views::ACTIONS {
            let shown = match a.id {
                // A control per editable setting: Turn on/off, a choice,
                // Save.
                "setting" => drawn
                    .iter()
                    .any(|t| t == "Turn on" || t == "Turn off" || t == "Save"),
                _ => drawn.iter().any(|t| t == a.label),
            };
            assert!(
                shown,
                "the window offers no control for {} ({:?})",
                a.id, a.label
            );
            let variant = match a.id {
                "pause" => "Act::Pause",
                "resume" => "Act::Resume",
                "reconnect" => "Act::Reconnect",
                "recheck" => "Act::Recheck",
                "setting" => "Act::Setting {",
                "setting_clear" => "Act::SettingClear {",
                "stop_session" => "Act::StopSession(",
                "report" => "Act::Report",
                "stop_node" => "Act::StopNode",
                "view_web" => "Act::View(\"web\"",
                "view_app" => "Act::View(\"app\"",
                "view_terminal" => "Act::View(\"terminal\"",
                "router_new_key" => "Act::RouterNewKey",
                "router_join" => "Act::RouterJoin(",
                "link_code" => "Act::LinkCode {",
                other => panic!("an action the window does not know: {other}"),
            };
            assert!(
                SRC.contains(variant),
                "no {variant} in the window for {}",
                a.id
            );
        }
    }

    #[test]
    fn the_setting_clear_control_shows_for_an_override() {
        let mut m = rich_model();
        let rows = m["settings"].as_array_mut().unwrap();
        for r in rows.iter_mut() {
            if r["key"] == "country" {
                r["overridden"] = json!(true);
            }
        }
        let mut app = App::with_model(m, None);
        let texts = render(&mut app, "settings", 1280.0);
        assert!(texts.iter().any(|t| t == "Use environment"));
    }

    #[test]
    fn a_recheck_stays_busy_until_a_newer_report_arrives() {
        let t0 = Instant::now();
        // Nothing asked, nothing running.
        assert!(!recheck_busy(None, false, Some(100), t0));
        // The node reports a running check.
        assert!(recheck_busy(None, true, Some(100), t0));
        // Asked here; the node has not picked it up yet: still busy.
        let asked = Some((t0, Some(100)));
        assert!(recheck_busy(
            asked,
            false,
            Some(100),
            t0 + Duration::from_secs(1)
        ));
        // Running, then a newer report: done.
        assert!(recheck_busy(
            asked,
            true,
            Some(100),
            t0 + Duration::from_secs(5)
        ));
        assert!(!recheck_busy(
            asked,
            false,
            Some(160),
            t0 + Duration::from_secs(20)
        ));
        // A first report where there was none counts as newer.
        assert!(!recheck_busy(Some((t0, None)), false, Some(1), t0));
        // A node that never answers does not lock the button forever.
        assert!(!recheck_busy(
            asked,
            false,
            Some(100),
            t0 + RECHECK_PATIENCE
        ));
    }

    /// Clicking Re-check now in the window drops the node's own recheck
    /// command into its control directory, and the button shows the check
    /// as busy until a newer report is published.
    #[test]
    fn the_recheck_button_sends_the_command_and_waits_for_the_report() {
        let dir = std::env::temp_dir().join(format!("kn-native-recheck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = rich_model();
        m["trust"]["checked_at"] = json!(100);
        let mut app = App::with_model(m, None);
        app.node_dir = dir.clone();
        app.act(Act::Recheck);
        let frames: Vec<Value> = std::fs::read_dir(crate::control::control_dir(&dir))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap())
            .collect();
        assert_eq!(frames, vec![crate::control::Command::Recheck.as_frame()]);
        assert!(app.recheck_busy(Instant::now()));
        let texts = render(&mut app, "trust", 1280.0);
        assert!(texts.iter().any(|t| t == "Checking..."), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Re-check now"));
        // The newer report arrives.
        app.model["trust"]["checked_at"] = json!(170);
        assert!(!app.recheck_busy(Instant::now()));
        let texts = render(&mut app, "trust", 1280.0);
        assert!(texts.iter().any(|t| t == "Re-check now"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_narrow_window_still_draws_every_section() {
        let mut app = App::with_model(rich_model(), None);
        for id in section_ids() {
            let texts = render(&mut app, id, 760.0);
            assert!(!texts.is_empty(), "{id}");
        }
    }

    #[test]
    fn the_router_section_explains_how_to_turn_it_on_and_shows_peers() {
        let mut app = App::with_model(rich_model(), None);
        let texts = render(&mut app, "router", 1280.0).join("\n");
        assert!(texts.contains("The router is off"), "{texts}");
        for control in [
            "Off",
            "This computer only",
            "On the network",
            "Join a cluster",
        ] {
            assert!(texts.contains(control), "{control} missing in:\n{texts}");
        }

        let mut r = RouterState {
            mode: crate::router::Mode::Lan,
            asked: crate::router::Mode::Lan,
            listening: "0.0.0.0:14418".into(),
            key_fingerprint: "abcd1234".into(),
            allow: vec!["192.168.1.0/24".into()],
            name: "studio".into(),
            discovery: "announcing as studio".into(),
            published_at_ms: crate::status::now_ms(),
            local: Some(crate::router::NodeReport {
                name: "studio".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        r.peers.push(crate::router::Peer {
            name: "attic".into(),
            address: "192.168.1.30".into(),
            state: PeerState::Unauthorized,
            detail: "it refused this cluster's key".into(),
            ..Default::default()
        });
        let mut app = App::with_model(rich_model(), Some(r));
        let texts = render(&mut app, "router", 1280.0).join("\n");
        for want in [
            "studio (this node)",
            "attic 192.168.1.30",
            "key refused",
            "fingerprint abcd1234",
        ] {
            assert!(texts.contains(want), "{want} missing in:\n{texts}");
        }
    }

    #[test]
    fn the_window_draws_only_glyphs_inter_has() {
        // Characters outside ASCII the window's own source uses in strings
        // must be in the bundled Latin subset (checked once, by hand, against
        // its cmap): a missing glyph would draw as a box.
        const SRC: &str = include_str!("mod.rs");
        const COVERED: &str = "·…—–•×€£°äöüßéÄÖÜ↑↓";
        for c in SRC.chars().filter(|c| !c.is_ascii()) {
            assert!(
                COVERED.contains(c),
                "{c:?} (U+{:04X}) is not in the font subset",
                c as u32
            );
        }
    }

    #[test]
    fn no_display_no_window() {
        // On Linux without DISPLAY/WAYLAND_DISPLAY there is no window; the
        // node then falls back to the browser app window.
        if cfg!(target_os = "linux")
            && std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
        {
            assert!(!display_available());
            assert_eq!(run(std::env::temp_dir(), String::new()), EXIT_NO_DISPLAY);
        }
        if cfg!(any(target_os = "macos", windows)) {
            assert!(display_available());
        }
    }
}
