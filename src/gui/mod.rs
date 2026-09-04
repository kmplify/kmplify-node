//! `kmplify-node gui` — the desktop window for the LAN router.
//!
//! The screens are adapted from NVIDIA's Personal AI Router (Apache-2.0,
//! see NOTICE): an overview of every node on the network with live meters,
//! the jobs that ran and where, the endpoints an application should point
//! at, and the settings. What PAIR renders in Electron over a JSON-RPC
//! bridge is drawn here with egui, immediate mode, from one clone of the
//! shared [`Router`] per frame — there is no bridge to keep in step because
//! there is no second process.
//!
//! Like the terminal dashboard it either **attaches** to the fabric node
//! already running here or **starts** one, so the fabric side of the window
//! (link state, delivered jobs) is the same node `kmplify-node status`
//! reports. The router side (discovery, meters, routing) lives in this
//! process either way.

mod chart;
mod theme;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, Pos2, RichText, Sense, Stroke, Vec2};

use kmplify_node::control::{self, Command};
use kmplify_node::fabric_worker::WorkerConfig;
use kmplify_node::gpu::{Backend, Gpu};
use kmplify_node::router::{self, Job, JobState, Node, Router, Shared, Source};
use kmplify_node::settings::Settings;
use kmplify_node::status::{self, Link, Snapshot};

/// Repaint cadence while nothing is happening. Meters move once a second;
/// twice that keeps a click feeling immediate without the window showing
/// up in its own CPU figure.
const REPAINT: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(1000);
const NOTICE_TTL: Duration = Duration::from_secs(6);

const JOBS_WIDTH: f32 = 270.0;
const LEGEND_WIDTH: f32 = 210.0;
const CHART_HEIGHT: f32 = 168.0;

/// Entry point for `kmplify-node gui`. Blocks on the window; returns the
/// process exit code once it closes.
pub async fn main(
    cfg: WorkerConfig,
    dir: PathBuf,
    attach: bool,
    standalone: bool,
    gpus: Vec<Gpu>,
) -> i32 {
    let live = match status::read_published_result(&dir) {
        Ok(snap) => snap.filter(Snapshot::is_fresh),
        Err(e) => {
            eprintln!(
                "cannot read {}: {e}\nA node is installed here but publishes as another user.",
                status::status_path(&dir).display()
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
        _ => live.is_some(),
    };

    let accel = cfg.accel();
    let engine_base = cfg.ollama_base.clone();
    let node = if attach_mode {
        None
    } else {
        // The window owns stdout's attention now; worker logs go to the ring
        // the log panel reads rather than scrolling under the window.
        status::set_quiet(true);
        Some(crate::start_node(cfg, dir.clone()).await)
    };

    let shared = router::new_shared(&dir, &gpus, router::lan_address());
    router::spawn(shared.clone(), accel);

    let app = GuiApp::new(shared, dir.clone(), attach_mode, accel, engine_base, live);
    let title = format!("KMPLIFY Node · {}", router::hostname());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    // eframe owns the main thread until the window closes. The runtime's
    // other workers keep the samplers and the fabric worker running
    // meanwhile; block_in_place tells tokio this thread is gone for a while
    // so it does not wait on it.
    let result = tokio::task::block_in_place(|| {
        eframe::run_native(
            &title,
            options,
            Box::new(|cc| {
                theme::apply(&cc.egui_ctx);
                Ok(Box::new(app))
            }),
        )
    });
    status::set_quiet(false);
    if let Some(node) = node {
        println!("[kmplify-node] window closed — stopping the node it started…");
        node.shutdown().await;
        println!("[kmplify-node] stopped cleanly");
    }
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("kmplify-node gui: {e}");
            1
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Settings,
}

/// Which job states the jobs column shows. All on by default, as in PAIR.
struct JobsFilter {
    queued: bool,
    running: bool,
    completed: bool,
    failed: bool,
}

impl JobsFilter {
    fn shows(&self, s: JobState) -> bool {
        match s {
            JobState::Queued => self.queued,
            JobState::Running => self.running,
            JobState::Completed => self.completed,
            JobState::Failed => self.failed,
        }
    }
}

/// The settings screen's draft, as concrete values. `Settings` stores
/// options (unset = the environment decides); the form needs a value in
/// every field, so it starts from what the node is actually applying.
#[derive(Clone, PartialEq)]
struct Form {
    share_inference: bool,
    share_cpu: bool,
    manual_approval: bool,
    country: String,
    engine: String,
    functions: bool,
    functions_pubkey: String,
    share_vectors: bool,
    /// 0 = no ceiling, which `set --clear` spells as unset.
    max_vram_mb: u64,
    max_ram_mb: u64,
    max_cpus: u32,
}

impl Form {
    fn from_node(snap: &Snapshot, stored: &Settings, engine_base: &str) -> Self {
        Self {
            share_inference: stored.share_inference.unwrap_or(snap.share_inference),
            share_cpu: stored.share_cpu.unwrap_or(snap.share_cpu),
            manual_approval: stored
                .approval_mode
                .as_deref()
                .unwrap_or(&snap.approval_mode)
                == "manual",
            country: stored.country.clone().unwrap_or_else(|| snap.country.clone()),
            engine: stored
                .engine
                .clone()
                .unwrap_or_else(|| engine_base.to_string()),
            functions: stored.functions.unwrap_or(snap.functions_enabled),
            functions_pubkey: stored
                .functions_pubkey
                .clone()
                .unwrap_or_else(|| snap.functions_pubkey.clone()),
            share_vectors: stored.share_vectors.unwrap_or(snap.vectors_enabled),
            max_vram_mb: stored.max_vram_mb.or(snap.max_vram_mb).unwrap_or(0),
            max_ram_mb: stored.max_ram_mb.or(snap.max_ram_mb).unwrap_or(0),
            max_cpus: stored
                .max_cpus
                .or(snap.max_cpus)
                .map(|c| c.round() as u32)
                .unwrap_or(0),
        }
    }

    /// Write the draft through the same `Settings::set` the CLI uses, so a
    /// value the CLI would refuse is refused here with the same words.
    fn save(&self, dir: &std::path::Path) -> Result<(), String> {
        let mut s = Settings::load(dir);
        s.set("share-inference", &self.share_inference.to_string())?;
        s.set("share-cpu", &self.share_cpu.to_string())?;
        s.set(
            "approval-mode",
            if self.manual_approval { "manual" } else { "auto" },
        )?;
        s.set("country", self.country.trim())?;
        if !self.engine.trim().is_empty() {
            s.set("engine", self.engine.trim())?;
        }
        s.set("functions", &self.functions.to_string())?;
        if self.functions_pubkey.trim().is_empty() {
            let _ = s.clear("functions-pubkey");
        } else {
            s.set("functions-pubkey", self.functions_pubkey.trim())?;
        }
        s.set("share-vectors", &self.share_vectors.to_string())?;
        ceiling(&mut s, "max-vram-mb", self.max_vram_mb as u64)?;
        ceiling(&mut s, "max-ram-mb", self.max_ram_mb as u64)?;
        ceiling(&mut s, "max-cpus", self.max_cpus as u64)?;
        s.save(dir)
            .map_err(|e| format!("cannot write {}: {e}", kmplify_node::settings::path(dir).display()))
    }
}

fn ceiling(s: &mut Settings, key: &str, value: u64) -> Result<(), String> {
    if value == 0 {
        // Clearing an override that was never set is not an error worth
        // showing anyone.
        let _ = s.clear(key);
        Ok(())
    } else {
        s.set(key, &value.to_string())
    }
}

struct GuiApp {
    shared: Shared,
    node_dir: PathBuf,
    attached: bool,
    accel: Backend,
    tab: Tab,
    filter: JobsFilter,
    form: Form,
    saved_form: Form,
    /// The last fabric snapshot, and the counters it had, so a finished
    /// fabric job becomes a card in the jobs column.
    snap: Snapshot,
    seen_done: u64,
    seen_failed: u64,
    last_poll: Instant,
    /// Per node: the legend entry soloed on its chart.
    solo: HashMap<String, String>,
    /// Per node: whether the engine section is open.
    engines_open: HashMap<String, bool>,
    add_node: Option<String>,
    endpoints_open: bool,
    chat_open: bool,
    chat: Chat,
    notice: Option<(String, Color32, Instant)>,
    /// The join form on the cluster card, and the attempt in flight.
    join_addr: String,
    join_pin: String,
    join_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Result<String, String>>>,
}

impl GuiApp {
    fn new(
        shared: Shared,
        node_dir: PathBuf,
        attached: bool,
        accel: Backend,
        engine_base: String,
        live: Option<Snapshot>,
    ) -> Self {
        let stored = Settings::load(&node_dir);
        let snap = live.unwrap_or_else(status::snapshot);
        let form = Form::from_node(&snap, &stored, &engine_base);
        Self {
            shared,
            node_dir,
            attached,
            accel,
            tab: Tab::Overview,
            filter: JobsFilter {
                queued: true,
                running: true,
                completed: true,
                failed: true,
            },
            saved_form: form.clone(),
            form,
            seen_done: snap.jobs.done,
            seen_failed: snap.jobs.failed,
            snap,
            last_poll: Instant::now() - POLL,
            solo: HashMap::new(),
            engines_open: HashMap::new(),
            add_node: None,
            endpoints_open: false,
            chat_open: false,
            chat: Chat::default(),
            notice: None,
            join_addr: String::new(),
            join_pin: String::new(),
            join_rx: None,
        }
    }

    fn notify(&mut self, text: impl Into<String>, color: Color32) {
        self.notice = Some((text.into(), color, Instant::now()));
    }

    /// Re-read the fabric node once a second and turn its counters into
    /// cards: the snapshot says how many jobs finished and which model was
    /// last, which is enough for "model X ran here at T" without the
    /// fabric worker ever handing prompts to anyone.
    fn poll(&mut self) {
        if self.last_poll.elapsed() < POLL {
            return;
        }
        self.last_poll = Instant::now();
        let fresh = if self.attached {
            status::read_published(&self.node_dir).filter(Snapshot::is_fresh)
        } else {
            Some(status::snapshot())
        };
        let Some(snap) = fresh else {
            self.snap.link = Link::Stopped;
            return;
        };
        let (self_id, local_name, gateway) = {
            let r = router::lock(&self.shared);
            (
                r.self_id.clone(),
                r.local().map(|n| n.name.clone()).unwrap_or_default(),
                snap.gateway
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_string(),
            )
        };
        let fabric_job = |i: u64, state: JobState, error: &str| Job {
            id: format!("{}-fabric-{i}", &self_id[..8.min(self_id.len())]),
            model: snap.jobs.last_model.clone(),
            engine: "fabric".into(),
            requested_from: gateway.clone(),
            ran_on: local_name.clone(),
            node_id: self_id.clone(),
            state,
            at_ms: status::now_ms(),
            error: error.into(),
            // Finished already: it must never count as pending here.
            local_origin: false,
        };
        let mut new_jobs = Vec::new();
        if snap.jobs.done > self.seen_done {
            for i in self.seen_done..snap.jobs.done {
                new_jobs.push(fabric_job(i, JobState::Completed, ""));
            }
        }
        if snap.jobs.failed > self.seen_failed {
            for i in self.seen_failed..snap.jobs.failed {
                new_jobs.push(fabric_job(
                    1_000_000 + i,
                    JobState::Failed,
                    "the fabric reported an error for this job",
                ));
            }
        }
        if !new_jobs.is_empty() {
            let mut r = router::lock(&self.shared);
            for j in new_jobs {
                r.push_job(j);
            }
        }
        self.seen_done = snap.jobs.done;
        self.seen_failed = snap.jobs.failed;
        self.snap = snap;
    }

    fn apply_settings(&mut self) {
        match self.form.save(&self.node_dir) {
            Ok(()) => {
                self.saved_form = self.form.clone();
                let nudged = status::read_published(&self.node_dir)
                    .filter(Snapshot::is_fresh)
                    .is_some()
                    && control::request(&self.node_dir, &Command::Reload).is_ok();
                self.notify(
                    if nudged {
                        "settings saved — the node re-advertises within a second"
                    } else {
                        "settings saved — applied when a node starts"
                    },
                    theme::OK,
                );
            }
            Err(e) => self.notify(e, theme::ERR),
        }
    }

    fn add_manual_node(&mut self, address: String) {
        let address = address.trim().to_string();
        if address.is_empty() {
            return;
        }
        let mut r = router::lock(&self.shared);
        if r.manual.iter().any(|a| a == &address) {
            drop(r);
            self.notify(format!("{address} is already on the list"), theme::WARN);
            return;
        }
        r.manual.push(address.clone());
        let id = format!("manual:{address}");
        let (host, port) = Node::parse_address(&address);
        let mut node = Node::new_peer(id.clone(), address.clone(), host, Source::Manual, Instant::now());
        node.info_port = port;
        // Offline until it answers a node-info poll, which is due at once.
        node.last_seen = Instant::now() - router::PEER_TIMEOUT;
        r.upsert_peer(node);
        r.push_log(format!("added {address} by hand; probing its node-info surface"));
        drop(r);
        self.notify(format!("added {address} — probing"), theme::OK);
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(REPAINT);
        self.poll();
        if let Some((_, _, at)) = &self.notice {
            if at.elapsed() > NOTICE_TTL {
                self.notice = None;
            }
        }
        let view: Router = router::lock(&self.shared).clone();

        self.top_bar(ui);
        self.status_bar(ui, &view);
        match self.tab {
            Tab::Overview => {
                self.jobs_column(ui, &view);
                self.node_list(ui, &view);
            }
            Tab::Settings => self.settings_screen(ui, &view),
        }
        self.add_node_window(&ctx);
        self.endpoints_window(&ctx, &view);
        self.chat_window(&ctx, &view);
    }
}

// ------------------------------------------------------------------ chrome

impl GuiApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(egui::Margin::symmetric(20, 12));
        egui::Panel::top("top").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                // The mark is painted: egui's bundled font has no U+25C6,
                // and a missing glyph draws as a box.
                let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                let c = r.center();
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(c.x, c.y - 7.0),
                        Pos2::new(c.x + 7.0, c.y),
                        Pos2::new(c.x, c.y + 7.0),
                        Pos2::new(c.x - 7.0, c.y),
                    ],
                    theme::PRIMARY,
                    Stroke::NONE,
                ));
                ui.label(RichText::new("KMPLIFY Node").size(17.0).strong());
                ui.label(theme::muted("personal inference router"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(theme::dim(kmplify_node::version_string()));
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                for (tab, label) in [(Tab::Overview, "Overview"), (Tab::Settings, "Settings")] {
                    let active = self.tab == tab;
                    let text = RichText::new(label)
                        .size(13.5)
                        .color(if active { theme::TEXT } else { theme::TEXT_MUTED })
                        .strong();
                    let resp = ui.add(
                        egui::Button::new(text)
                            .fill(if active { theme::CARD_RAISED } else { Color32::TRANSPARENT })
                            .stroke(Stroke::NONE),
                    );
                    if active {
                        let r = resp.rect;
                        ui.painter().line_segment(
                            [
                                Pos2::new(r.left() + 4.0, r.bottom() + 3.0),
                                Pos2::new(r.right() - 4.0, r.bottom() + 3.0),
                            ],
                            Stroke::new(2.0, theme::PRIMARY),
                        );
                    }
                    if resp.clicked() {
                        self.tab = tab;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let add = ui.add(
                        egui::Button::new(RichText::new("+ Add node").color(Color32::WHITE).strong())
                            .fill(theme::PRIMARY)
                            .stroke(Stroke::NONE),
                    );
                    if add.clicked() {
                        self.add_node = Some(String::new());
                    }
                    if ui.button("Endpoints").clicked() {
                        self.endpoints_open = !self.endpoints_open;
                    }
                    if ui.button("Chat").clicked() {
                        self.chat_open = !self.chat_open;
                    }
                });
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, view: &Router) {
        let frame = egui::Frame::new()
            .fill(theme::PANEL)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .inner_margin(egui::Margin::symmetric(16, 6));
        egui::Panel::bottom("status").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (label, color) = match self.snap.link {
                    Link::Online if self.snap.paused => ("PAUSED", theme::WARN),
                    Link::Online => ("ONLINE", theme::OK),
                    Link::Connecting | Link::Starting => (self.snap.link.label(), theme::WARN),
                    Link::Retrying => ("RETRYING", theme::ERR),
                    Link::Stopping | Link::Stopped => ("NO NODE", theme::ERR),
                };
                ui.label(RichText::new("fabric").color(theme::TEXT_MUTED).size(12.0));
                theme::pill(ui, label, color);
                if !self.snap.node_id.is_empty() {
                    ui.label(theme::dim(format!(
                        "node {}",
                        &self.snap.node_id[..12.min(self.snap.node_id.len())]
                    )));
                }
                ui.label(theme::dim(format!(
                    "jobs {} finished · {} errors",
                    self.snap.jobs.done, self.snap.jobs.failed
                )));
                ui.separator();
                ui.label(RichText::new("network").color(theme::TEXT_MUTED).size(12.0));
                ui.label(theme::dim(&view.discovery));
                let peers = view.nodes.values().filter(|n| !n.is_local()).count();
                ui.label(theme::dim(format!("{peers} peer(s)")));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some((text, color, _)) = &self.notice {
                        ui.label(RichText::new(text).color(*color).size(12.0));
                    } else {
                        ui.label(theme::dim(if self.attached {
                            "attached to the node running here"
                        } else {
                            "running the node in this window"
                        }));
                    }
                });
            });
        });
    }
}

// ------------------------------------------------------------------ jobs

impl GuiApp {
    fn jobs_column(&mut self, ui: &mut egui::Ui, view: &Router) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(egui::Margin {
                left: 20,
                right: 8,
                top: 8,
                bottom: 8,
            });
        egui::Panel::left("jobs")
            .exact_size(JOBS_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(frame)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.menu_button(RichText::new("⏷ Jobs").strong(), |ui| {
                        ui.checkbox(&mut self.filter.queued, "Queued");
                        ui.checkbox(&mut self.filter.running, "Running");
                        ui.checkbox(&mut self.filter.completed, "Completed");
                        ui.checkbox(&mut self.filter.failed, "Failed");
                    });
                    if self.snap.jobs.active > 0 {
                        theme::pill(ui, &format!("{} running", self.snap.jobs.active), theme::OK);
                    }
                });
                ui.add_space(6.0);
                let shown: Vec<&Job> = view
                    .jobs
                    .iter()
                    .filter(|j| self.filter.shows(j.state))
                    .collect();
                if shown.is_empty() {
                    theme::card_low().show(ui, |ui| {
                        ui.label(theme::muted("No jobs yet."));
                        ui.label(theme::dim(
                            "Requests that reach the router's endpoints appear here as they \
                             run, naming the node that served them; so does work the fabric \
                             sends this node.",
                        ));
                    });
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("jobs-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for job in shown {
                            job_card(ui, job);
                            ui.add_space(6.0);
                        }
                    });
            });
    }
}

fn job_card(ui: &mut egui::Ui, job: &Job) {
    let (bar, state) = match job.state {
        JobState::Queued => (theme::WARN, "queued"),
        JobState::Running => (theme::OK, "running"),
        JobState::Completed => (theme::TEXT_DIM, "completed"),
        JobState::Failed => (theme::ERR, "failed"),
    };
    let frame = if matches!(job.state, JobState::Completed | JobState::Failed) {
        theme::card_low()
    } else {
        theme::card()
    };
    let resp = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("▣").color(theme::TEXT_MUTED).size(12.0));
            ui.label(
                RichText::new(if job.model.is_empty() { "(model unknown)" } else { &job.model })
                    .strong()
                    .size(13.0),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                theme::pill(ui, state, bar);
            });
        });
        if !job.requested_from.is_empty() {
            ui.label(theme::muted(format!("Requested from {}", job.requested_from)));
        }
        if !job.ran_on.is_empty() {
            let verb = if job.state == JobState::Running { "Running on" } else { "Ran on" };
            ui.label(theme::muted(format!("{verb} {}", job.ran_on)));
        }
        ui.label(theme::muted(format!(
            "{} {}",
            job.state.label(),
            status::clock_hms(job.at_ms)
        )));
        if !job.error.is_empty() {
            ui.label(RichText::new(&job.error).color(theme::ERR).size(12.0));
        }
    });
    // The coloured edge PAIR's cards carry: state at a glance, before the
    // text is read.
    let r = resp.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(r.left_top(), Vec2::new(3.0, r.height())),
        theme::RADIUS_SM,
        bar,
    );
}

// ------------------------------------------------------------------ nodes

impl GuiApp {
    fn node_list(&mut self, ui: &mut egui::Ui, view: &Router) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(egui::Margin {
                left: 8,
                right: 20,
                top: 8,
                bottom: 8,
            });
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("nodes-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let now = Instant::now();
                    for node in view.ordered() {
                        let paired = view.is_member(&node.id);
                        self.node_card(ui, node, now, paired);
                        ui.add_space(10.0);
                    }
                    if view.nodes.len() == 1 {
                        theme::card_low().show(ui, |ui| {
                            ui.label(theme::muted(
                                "Only this machine so far. Run `kmplify-node gui` on another \
                                 computer on this network and it appears here; or add one by \
                                 address with + Add node.",
                            ));
                        });
                    }
                });
        });
    }

    fn node_card(&mut self, ui: &mut egui::Ui, node: &Node, now: Instant, paired: bool) {
        let online = node.online(now);
        let m = &node.metrics;
        let has_gpu = !node.gpus.is_empty();
        let gpu = node.gpus.first();
        let base_color = if has_gpu { theme::GPU } else { theme::CPU };

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            // ---- header: ring, name, engines
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                let outer = if has_gpu {
                    m.gpu_known.then(|| m.gpu.latest() / 100.0)
                } else {
                    m.sampled.then(|| m.cpu.latest() / 100.0)
                };
                let inner = if has_gpu {
                    m.vram_known.then(|| m.vram.latest() / 100.0)
                } else {
                    m.sampled.then(|| m.ram.latest() / 100.0)
                };
                let (c1, c2) = if has_gpu {
                    (theme::GPU, theme::VRAM)
                } else {
                    (theme::CPU, theme::RAM)
                };
                if online && (m.sampled || m.gpu_known) {
                    ui.painter().circle_stroke(
                        rect.center(),
                        30.0,
                        Stroke::new(1.0, theme::with_alpha(base_color, 0x50)),
                    );
                }
                chart::rings(ui.painter(), rect.center(), 26.0, &[(outer, c1), (inner, c2)]);

                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(node.name.to_uppercase())
                                .strong()
                                .size(14.0)
                                .color(if online { theme::TEXT } else { theme::TEXT_MUTED }),
                        );
                        if node.is_local() {
                            theme::pill(ui, "LOCAL", theme::OK);
                        } else if paired {
                            theme::pill(ui, "PAIRED", theme::PRIMARY);
                        } else if !node.cluster_id.is_empty() {
                            theme::pill(ui, "OTHER CLUSTER", theme::WARN);
                        } else {
                            theme::pill(ui, "UNPAIRED", theme::TEXT_MUTED);
                        }
                        if !online && !node.is_local() {
                            theme::pill(ui, "OFFLINE", theme::TEXT_DIM);
                        }
                        if node.source == Source::Manual {
                            theme::pill(ui, "BY ADDRESS", theme::TEXT_DIM);
                        }
                        ui.label(theme::muted(format!("· {}", node.address)));
                        if let Some(g) = gpu {
                            ui.label(theme::muted(format!("· {}", g.name)));
                        }
                    });
                    ui.horizontal(|ui| {
                        engine_badges(ui, node);
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let open = self.engines_open.get(&node.id).copied().unwrap_or(false);
                    if ui
                        .add(egui::Button::new(RichText::new("⚙").size(14.0)).fill(if open {
                            theme::PRIMARY_STRONG
                        } else {
                            theme::CARD_RAISED
                        }))
                        .on_hover_text("Engine settings")
                        .clicked()
                    {
                        self.engines_open.insert(node.id.clone(), !open);
                    }
                });
            });

            ui.add_space(10.0);

            // ---- chart + legend
            ui.horizontal(|ui| {
                let chart_w = (ui.available_width() - LEGEND_WIDTH - 12.0).max(200.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(chart_w, CHART_HEIGHT), Sense::hover());
                let solo = self.solo.get(&node.id).map(|s| s.as_str());
                let mut lines = Vec::new();
                if has_gpu && m.gpu_known {
                    lines.push(chart::Line {
                        key: "gpu",
                        color: theme::GPU,
                        points: m.gpu.points().collect(),
                    });
                }
                if has_gpu && m.vram_known {
                    lines.push(chart::Line {
                        key: "vram",
                        color: theme::VRAM,
                        points: m.vram.points().collect(),
                    });
                }
                if m.sampled {
                    lines.push(chart::Line {
                        key: "cpu",
                        color: theme::CPU,
                        points: m.cpu.points().collect(),
                    });
                    lines.push(chart::Line {
                        key: "ram",
                        color: theme::RAM,
                        points: m.ram.points().collect(),
                    });
                }
                chart::area_lines(ui, rect, &lines, solo);
                if lines.is_empty() {
                    let text = if !online {
                        "offline"
                    } else if node.is_local() {
                        "no metrics yet"
                    } else if node.poll_failures > 0 {
                        "node-info does not answer yet"
                    } else {
                        "waiting for the first node-info poll"
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        egui::FontId::proportional(12.0),
                        theme::TEXT_MUTED,
                    );
                }

                ui.add_space(12.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(LEGEND_WIDTH, CHART_HEIGHT),
                    Layout::top_down(Align::Min),
                    |ui| {
                        self.legend(ui, node);
                    },
                );
            });

            if self.engines_open.get(&node.id).copied().unwrap_or(false) {
                ui.add_space(8.0);
                engine_details(ui, node);
            }
        });
    }

    fn legend(&mut self, ui: &mut egui::Ui, node: &Node) {
        let m = &node.metrics;
        let solo = self.solo.get(&node.id).cloned();
        let mut clicked: Option<String> = None;
        let entry = |ui: &mut egui::Ui, key: &str, color: Color32, label: &str, value: String, clicked: &mut Option<String>| {
            let faded = solo.as_deref().is_some_and(|s| s != key);
            let resp = ui
                .horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                    ui.painter().circle(
                        r.center(),
                        5.0,
                        theme::with_alpha(color, if faded { 0x40 } else { 0xbf }),
                        Stroke::new(1.5, theme::with_alpha(color, if faded { 0x60 } else { 0xff })),
                    );
                    let c = if faded { theme::TEXT_DIM } else { theme::TEXT_MUTED };
                    ui.label(RichText::new(label).color(c).size(12.0));
                    ui.label(RichText::new(value).color(if faded { theme::TEXT_DIM } else { theme::TEXT }).size(12.0));
                })
                .response;
            if resp.interact(Sense::click()).clicked() {
                *clicked = Some(key.to_string());
            }
        };

        if let Some(g) = node.gpus.first() {
            ui.label(RichText::new(&g.name).strong().size(12.5));
            entry(
                ui,
                "gpu",
                theme::GPU,
                "Usage",
                if m.gpu_known { format!("{:.0}%", m.gpu.latest()) } else { "n/a".into() },
                &mut clicked,
            );
            entry(
                ui,
                "vram",
                theme::VRAM,
                "VRAM",
                if m.vram_known {
                    format!("{} / {}", gb(m.vram_used_mb), gb(g.total_mb))
                } else if g.total_mb > 0 {
                    format!("{} (unified)", gb(g.total_mb))
                } else {
                    "n/a".into()
                },
                &mut clicked,
            );
            ui.add_space(6.0);
        }
        ui.label(
            RichText::new(if node.cpu_model.is_empty() { "CPU" } else { &node.cpu_model })
                .strong()
                .size(12.5),
        );
        entry(
            ui,
            "cpu",
            theme::CPU,
            "Usage",
            if m.sampled {
                format!("{:.0}% ({} cores)", m.cpu.latest(), node.cpu_cores)
            } else if node.cpu_cores > 0 {
                format!("{} cores", node.cpu_cores)
            } else {
                "n/a".into()
            },
            &mut clicked,
        );
        ui.add_space(6.0);
        ui.label(RichText::new("Memory").strong().size(12.5));
        entry(
            ui,
            "ram",
            theme::RAM,
            "Used",
            if m.sampled {
                format!("{} / {}", gb(m.ram_used_mb), gb(node.ram_total_mb))
            } else if node.ram_total_mb > 0 {
                gb(node.ram_total_mb)
            } else {
                "n/a".into()
            },
            &mut clicked,
        );

        if let Some(key) = clicked {
            // Click the soloed entry again to show everything.
            if solo.as_deref() == Some(key.as_str()) {
                self.solo.remove(&node.id);
            } else {
                self.solo.insert(node.id.clone(), key);
            }
        }
    }
}

/// The engines a card advertises: the two PAIR knows plus whatever else
/// answers, running ones lit, the others dim. Dim rather than hidden, so
/// the card says what the machine could run, not only what it does.
fn engine_badges(ui: &mut egui::Ui, node: &Node) {
    let mut shown = 0;
    for e in &node.engines {
        let staple = matches!(e.id.as_str(), "ollama" | "lmstudio");
        if !e.running && !staple {
            continue;
        }
        shown += 1;
        let color = if e.running { theme::OK } else { theme::TEXT_DIM };
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, color);
            ui.label(
                RichText::new(&e.name)
                    .size(12.0)
                    .color(if e.running { theme::TEXT } else { theme::TEXT_DIM }),
            );
            if e.running && !e.models.is_empty() {
                ui.label(theme::dim(format!("{}", e.models.len())));
            }
        });
        ui.add_space(6.0);
    }
    if shown == 0 {
        ui.label(theme::dim("no engine answering"));
    }
}

fn engine_details(ui: &mut egui::Ui, node: &Node) {
    theme::card_low().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::heading("Engines"));
        for e in &node.engines {
            ui.horizontal(|ui| {
                let color = if e.running { theme::OK } else { theme::TEXT_DIM };
                let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, color);
                ui.label(RichText::new(&e.name).strong().size(12.5));
                ui.label(theme::muted(&e.base));
                ui.label(theme::dim(if e.running {
                    format!("{} model(s)", e.models.len())
                } else {
                    "not running".to_string()
                }));
            });
            let named: Vec<&String> = e.models.iter().filter(|m| !m.is_empty()).collect();
            if e.running && !named.is_empty() {
                ui.indent(&e.id, |ui| {
                    ui.label(theme::dim(
                        named.iter().map(|m| m.as_str()).collect::<Vec<_>>().join("  ·  "),
                    ));
                });
            }
        }
        if !node.is_local() {
            ui.label(theme::dim(
                "Starting, stopping and installing a peer's engine arrives with pairing (phase 3).",
            ));
        }
    });
}

fn gb(mb: u64) -> String {
    format!("{:.1} GB", mb as f64 / 1024.0)
}

// ------------------------------------------------------------------ settings

impl GuiApp {
    fn settings_screen(&mut self, ui: &mut egui::Ui, view: &Router) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(egui::Margin::symmetric(20, 8));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("settings-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_max_width(760.0);
                    let vram_total = view
                        .local()
                        .and_then(|n| n.gpus.first())
                        .map(|g| g.total_mb)
                        .unwrap_or(0);
                    let (ram_total, cores) = view
                        .local()
                        .map(|n| (n.ram_total_mb, n.cpu_cores))
                        .unwrap_or((0, 0));

                    // Pairing first: on this screen it is the action a new
                    // install is looking for.
                    self.cluster_card(ui, view);
                    ui.add_space(10.0);

                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(theme::heading("What this machine lends to the fabric"));
                        ui.label(theme::muted(
                            "The same switches as `kmplify-node set` and the terminal dashboard's \
                             sharing screen; all three write one file.",
                        ));
                        ui.add_space(6.0);
                        ui.checkbox(&mut self.form.share_inference, "GPU inference (chat and embeddings)");
                        ui.checkbox(&mut self.form.share_cpu, "Spare CPU threads and system RAM");
                        ui.checkbox(
                            &mut self.form.manual_approval,
                            "Require my approval for new peers (manual admission)",
                        );
                        ui.horizontal(|ui| {
                            ui.label("Country (ISO alpha-2, for EU residency filters)");
                            ui.add(egui::TextEdit::singleline(&mut self.form.country).desired_width(60.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Engine (name or URL)");
                            ui.add(egui::TextEdit::singleline(&mut self.form.engine).desired_width(320.0));
                        });
                    });
                    ui.add_space(10.0);

                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(theme::heading("Ceilings — peers never take more than this"));
                        ui.label(theme::muted("0 means no ceiling (the environment decides)."));
                        ui.add_space(6.0);
                        if vram_total > 0 {
                            ui.horizontal(|ui| {
                                ui.label("VRAM offered");
                                ui.add(
                                    egui::Slider::new(&mut self.form.max_vram_mb, 0..=vram_total)
                                        .custom_formatter(|v, _| slider_gb(v as u64))
                                        .step_by(512.0),
                                );
                            });
                        }
                        if ram_total > 0 {
                            ui.horizontal(|ui| {
                                ui.label("System RAM offered");
                                ui.add(
                                    egui::Slider::new(&mut self.form.max_ram_mb, 0..=ram_total)
                                        .custom_formatter(|v, _| slider_gb(v as u64))
                                        .step_by(1024.0),
                                );
                            });
                        }
                        if cores > 0 {
                            ui.horizontal(|ui| {
                                ui.label("CPU threads for peer sessions");
                                ui.add(egui::Slider::new(&mut self.form.max_cpus, 0..=cores as u32));
                            });
                        }
                    });
                    ui.add_space(10.0);

                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(theme::heading("Extra lanes (both off by default)"));
                        ui.checkbox(&mut self.form.functions, "Host signed Wasm functions");
                        ui.horizontal(|ui| {
                            ui.label("Catalog key to trust (hex)");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.functions_pubkey)
                                    .desired_width(480.0),
                            );
                        });
                        ui.checkbox(&mut self.form.share_vectors, "Hold peers' vector collections");
                    });
                    ui.add_space(10.0);

                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(theme::heading("This network"));
                        if let Some(me) = view.local() {
                            ui.label(theme::muted(format!(
                                "Advertising as {} ({}) on {} — id {}",
                                me.name,
                                me.address,
                                router::SERVICE_TYPE.trim_end_matches('.'),
                                &me.id[..12.min(me.id.len())]
                            )));
                        }
                        ui.label(theme::muted(format!(
                            "Discovery: {} · listeners: {}",
                            view.discovery, view.listeners
                        )));
                        let mut ingress = view.lan_ingress;
                        if ui
                            .checkbox(&mut ingress, "Serve requests from paired nodes")
                            .changed()
                        {
                            let mut r = router::lock(&self.shared);
                            r.lan_ingress = ingress;
                            r.push_log(if ingress {
                                "LAN ingress on: peers may use this node's engines"
                            } else {
                                "LAN ingress off: this node only consumes the cluster"
                            });
                        }
                        ui.label(theme::dim(
                            "What crosses the network: hostname, node id, hardware, which engines \
                             answer and their model names, how busy the machine is, and which jobs \
                             ran where. Never a request or a response. Requests between machines \
                             travel only between paired nodes, over mutual TLS. While this node is \
                             in no cluster its report is readable by anything on the subnet, so \
                             strangers can find each other to pair; once paired, only members read it.",
                        ));
                        if !view.manual.is_empty() {
                            ui.add_space(4.0);
                            ui.label(theme::muted("Nodes added by address:"));
                            let mut forget: Option<String> = None;
                            for a in &view.manual {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(a).size(12.5));
                                    if ui.small_button("forget").clicked() {
                                        forget = Some(a.clone());
                                    }
                                });
                            }
                            if let Some(a) = forget {
                                let mut r = router::lock(&self.shared);
                                r.manual.retain(|x| x != &a);
                                r.nodes.remove(&format!("manual:{a}"));
                            }
                        }
                    });
                    ui.add_space(12.0);

                    ui.horizontal(|ui| {
                        let dirty = self.form != self.saved_form;
                        let apply = ui.add_enabled(
                            dirty,
                            egui::Button::new(RichText::new("Apply").color(Color32::WHITE).strong())
                                .fill(theme::PRIMARY),
                        );
                        if apply.clicked() {
                            self.apply_settings();
                        }
                        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                            self.form = self.saved_form.clone();
                        }
                        if !self.attached {
                            ui.separator();
                            if ui.button(if self.snap.paused { "Resume sharing" } else { "Pause sharing" }).clicked() {
                                let cmd = if self.snap.paused { Command::Resume } else { Command::Pause };
                                match control::submit(&cmd) {
                                    Ok(()) => self.notify(cmd.confirmation(), theme::OK),
                                    Err(e) => self.notify(e, theme::ERR),
                                }
                            }
                            if ui.button("Reconnect").clicked() {
                                match control::submit(&Command::Reconnect) {
                                    Ok(()) => self.notify("reconnecting…", theme::OK),
                                    Err(e) => self.notify(e, theme::ERR),
                                }
                            }
                        }
                        ui.label(theme::dim(format!("accelerator: {}", self.accel.as_str())));
                    });
                    ui.add_space(20.0);
                });
        });
    }
}

fn slider_gb(mb: u64) -> String {
    if mb == 0 {
        "no ceiling".into()
    } else {
        gb(mb)
    }
}

// ------------------------------------------------------------------ cluster

impl GuiApp {
    /// Pairing and membership: the card PAIR puts under Settings → Cluster.
    fn cluster_card(&mut self, ui: &mut egui::Ui, view: &Router) {
        if let Some(rx) = &mut self.join_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(msg) => {
                        self.join_pin.clear();
                        self.notify(msg, theme::OK);
                    }
                    Err(e) => self.notify(format!("pairing failed: {e}"), theme::ERR),
                }
                self.join_rx = None;
            }
        }
        let now = Instant::now();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::heading("Cluster"));
            let Some(identity) = &view.identity else {
                ui.label(RichText::new(
                    "This node has no certificate, so it cannot pair. The log says why.",
                ).color(theme::ERR).size(12.5));
                return;
            };
            ui.label(theme::muted(format!(
                "This node's certificate fingerprint: {}…",
                kmplify_node::router::cluster::short_fp(&identity.fingerprint)
            )));
            if view.cluster.is_clustered() {
                ui.label(theme::muted(format!(
                    "Cluster {}… · {} paired node(s)",
                    &view.cluster.cluster_id[..8.min(view.cluster.cluster_id.len())],
                    view.cluster.members.len()
                )));
                let mut remove: Option<String> = None;
                for m in view.cluster.members.values() {
                    ui.horizontal(|ui| {
                        let online = view.nodes.get(&m.id).is_some_and(|n| n.online(now));
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                        ui.painter().circle_filled(
                            r.center(),
                            4.0,
                            if online { theme::OK } else { theme::TEXT_DIM },
                        );
                        ui.label(RichText::new(&m.name).strong().size(12.5));
                        ui.label(theme::dim(format!(
                            "{}… · {}…",
                            &m.id[..8.min(m.id.len())],
                            kmplify_node::router::cluster::short_fp(&m.fingerprint)
                        )));
                        if ui.small_button("remove").clicked() {
                            remove = Some(m.id.clone());
                        }
                    });
                }
                if let Some(id) = remove {
                    let mut r = router::lock(&self.shared);
                    r.remove_member(&id);
                    r.push_log(format!("removed {} from the cluster", &id[..8.min(id.len())]));
                }
            } else {
                ui.label(theme::muted(
                    "Not in a cluster. Invite another node, or join one with the PIN it shows.",
                ));
            }
            ui.add_space(6.0);

            // ---- invite
            match &view.invite {
                Some(inv) if !inv.expired() => {
                    ui.horizontal(|ui| {
                        ui.label(theme::muted("Invitation PIN"));
                        ui.label(
                            RichText::new(&inv.pin)
                                .size(26.0)
                                .strong()
                                .color(theme::PRIMARY)
                                .monospace(),
                        );
                        if let Some(me) = view.local() {
                            ui.label(theme::muted(format!(
                                "enter it on the other machine with this address: {}:{}",
                                me.address,
                                router::node_info_port()
                            )));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(theme::dim(format!(
                            "valid for {} more second(s), {} wrong attempt(s) so far",
                            inv.remaining().as_secs(),
                            inv.wrong_attempts
                        )));
                        if ui.small_button("cancel invitation").clicked() {
                            router::lock(&self.shared).invite = None;
                        }
                    });
                }
                _ => {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Invite a node").color(Color32::WHITE).strong())
                                .fill(theme::PRIMARY),
                        )
                        .clicked()
                    {
                        let pin = router::lock(&self.shared).open_invite();
                        self.notify(format!("invitation open: PIN {pin}"), theme::OK);
                    }
                }
            }
            ui.add_space(6.0);

            // ---- join
            ui.horizontal(|ui| {
                ui.label(theme::muted("Join"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.join_addr)
                        .hint_text("address of the inviting node")
                        .desired_width(220.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.join_pin)
                        .hint_text("PIN")
                        .desired_width(70.0),
                );
                let can = self.join_rx.is_none()
                    && !self.join_addr.trim().is_empty()
                    && self.join_pin.trim().len() == 6;
                if ui.add_enabled(can, egui::Button::new("Join")).clicked() {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    self.join_rx = Some(rx);
                    let shared = self.shared.clone();
                    let addr = self.join_addr.trim().to_string();
                    let pin = self.join_pin.trim().to_string();
                    let ctx = ui.ctx().clone();
                    tokio::spawn(async move {
                        let _ = tx.send(kmplify_node::router::cluster::join(shared, addr, pin).await);
                        ctx.request_repaint();
                    });
                }
                if self.join_rx.is_some() {
                    ui.label(theme::muted("pairing…"));
                }
            });
            if view.cluster.is_clustered() {
                ui.add_space(4.0);
                if ui.small_button("Leave cluster").clicked() {
                    let mut r = router::lock(&self.shared);
                    r.leave_cluster();
                    r.push_log("left the cluster; every pin dropped");
                }
            }
        });
    }
}

// ------------------------------------------------------------------ windows

impl GuiApp {
    fn add_node_window(&mut self, ctx: &egui::Context) {
        let Some(mut text) = self.add_node.clone() else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new("Add node")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(theme::muted(
                    "Some networks block multicast. Type the address of a machine running \
                     kmplify-node and it joins the list; it is probed and its card fills in \
                     once its node-info surface exists (phase 2).",
                ));
                ui.add_space(6.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut text)
                        .hint_text("192.168.1.25 or spark-ede9.local")
                        .desired_width(f32::INFINITY),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Add").color(Color32::WHITE).strong())
                                .fill(theme::PRIMARY),
                        )
                        .clicked()
                    {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        submit = false;
                        text.clear();
                    }
                });
            });
        if submit {
            self.add_manual_node(text);
            self.add_node = None;
        } else if !open || text.is_empty() && self.add_node.as_deref() != Some("") {
            self.add_node = None;
        } else {
            self.add_node = Some(text);
        }
    }

    fn endpoints_window(&mut self, ctx: &egui::Context, view: &Router) {
        if !self.endpoints_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Endpoints")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(theme::heading("Engines on this machine"));
                if let Some(me) = view.local() {
                    for e in me.running_engines() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&e.name).strong().size(12.5));
                            ui.monospace(&e.base);
                            if ui.small_button("copy").clicked() {
                                ui.ctx().copy_text(e.base.clone());
                            }
                        });
                    }
                    if me.running_engines().next().is_none() {
                        ui.label(theme::dim("nothing is answering right now"));
                    }
                }
                ui.label(theme::dim(
                    "Point an application at one of these and it reaches that engine only.",
                ));
                ui.add_space(8.0);
                ui.label(theme::heading("Router endpoints"));
                ui.label(theme::muted(
                    "One address per API. Each request goes to whichever node on this network \
                     serves the model, with failover; `/api/tags` and `/v1/models` list the \
                     whole network's inventory.",
                ));
                for (label, port) in [
                    ("Ollama-compatible", router::proxy_ollama_port()),
                    ("OpenAI-compatible", router::proxy_openai_port()),
                ] {
                    let url = format!("http://127.0.0.1:{port}");
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).size(12.5));
                        ui.monospace(&url);
                        if ui.small_button("copy").clicked() {
                            ui.ctx().copy_text(url.clone());
                        }
                    });
                }
                ui.label(theme::dim(format!(
                    "Listeners: {}. Loopback for applications on this machine; nodes on this \
                     network reach the same ports while LAN ingress is on (Settings).",
                    view.listeners
                )));
            });
        self.endpoints_open = open;
    }

    /// A chat that goes through the router's own OpenAI endpoint, so what
    /// you type here is routed exactly as an application's request would
    /// be — and shows up in the jobs column naming the node that answered.
    fn chat_window(&mut self, ctx: &egui::Context, view: &Router) {
        if !self.chat_open {
            return;
        }
        if let Some(rx) = &mut self.chat.rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(text) => self.chat.log.push((false, text)),
                    Err(e) => self.chat.log.push((false, format!("(error) {e}"))),
                }
                self.chat.rx = None;
            }
        }
        let models: Vec<String> = view
            .inventory(kmplify_node::router::proxy::Api::OpenAi, Instant::now())
            .into_keys()
            .collect();
        if self.chat.model.is_empty() {
            if let Some(m) = models.first() {
                self.chat.model = m.clone();
            }
        }
        let mut open = true;
        let mut send = false;
        egui::Window::new("Chat")
            .open(&mut open)
            .collapsible(false)
            .default_size([560.0, 480.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::muted("model"));
                    egui::ComboBox::from_id_salt("chat-model")
                        .selected_text(if self.chat.model.is_empty() {
                            "none on the network".to_string()
                        } else {
                            self.chat.model.clone()
                        })
                        .show_ui(ui, |ui| {
                            for m in &models {
                                ui.selectable_value(&mut self.chat.model, m.clone(), m);
                            }
                        });
                    if ui.small_button("clear").clicked() {
                        self.chat.log.clear();
                    }
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("chat-scroll")
                    .max_height(300.0)
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (mine, text) in &self.chat.log {
                            let frame = if *mine { theme::card() } else { theme::card_low() };
                            frame.show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(theme::dim(if *mine { "you" } else { "router" }));
                                ui.label(RichText::new(text).size(13.0));
                            });
                            ui.add_space(4.0);
                        }
                        if self.chat.rx.is_some() {
                            ui.label(theme::muted("…"));
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.chat.input)
                            .hint_text("Ask the network")
                            .desired_width(ui.available_width() - 70.0),
                    );
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                    }
                    let can = self.chat.rx.is_none()
                        && !self.chat.model.is_empty()
                        && !self.chat.input.trim().is_empty();
                    if ui
                        .add_enabled(
                            can,
                            egui::Button::new(RichText::new("Send").color(Color32::WHITE).strong())
                                .fill(theme::PRIMARY),
                        )
                        .clicked()
                    {
                        send = true;
                    }
                });
            });
        if send && self.chat.rx.is_none() && !self.chat.model.is_empty() {
            let text = self.chat.input.trim().to_string();
            if !text.is_empty() {
                self.chat.input.clear();
                self.chat.log.push((true, text));
                let messages: Vec<serde_json::Value> = self
                    .chat
                    .log
                    .iter()
                    .filter(|(_, t)| !t.starts_with("(error)"))
                    .map(|(mine, t)| {
                        serde_json::json!({ "role": if *mine { "user" } else { "assistant" }, "content": t })
                    })
                    .collect();
                let body = serde_json::json!({
                    "model": self.chat.model,
                    "messages": messages,
                    "stream": false,
                });
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                self.chat.rx = Some(rx);
                let ctx2 = ctx.clone();
                tokio::spawn(async move {
                    let url = format!(
                        "http://127.0.0.1:{}/v1/chat/completions",
                        router::proxy_openai_port()
                    );
                    let result = async {
                        let resp = reqwest::Client::new()
                            .post(&url)
                            .json(&body)
                            .timeout(Duration::from_secs(600))
                            .send()
                            .await
                            .map_err(|e| e.to_string())?;
                        let status = resp.status();
                        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
                        if !status.is_success() {
                            let msg = v["error"]["message"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string());
                            return Err(format!("{status}: {msg}"));
                        }
                        v["choices"][0]["message"]["content"]
                            .as_str()
                            .map(|s| s.trim().to_string())
                            .ok_or_else(|| "no content in the answer".to_string())
                    }
                    .await;
                    let _ = tx.send(result);
                    ctx2.request_repaint();
                });
            }
        }
        self.chat_open = open;
    }
}

/// The chat pane's state: the conversation, the model, and the reply in
/// flight. The conversation lives only in this window and dies with it.
#[derive(Default)]
struct Chat {
    model: String,
    input: String,
    log: Vec<(bool, String)>,
    rx: Option<tokio::sync::mpsc::UnboundedReceiver<Result<String, String>>>,
}
