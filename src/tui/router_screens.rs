//! The LAN router's two terminal screens: `8 network` and `9 cluster`.
//!
//! A router node usually runs where there is no desktop, and PAIR's own
//! answer to that is a terminal interface with the same tabs as its app.
//! These are the desktop window's Overview and Cluster card, in the
//! dashboard's idiom: one screen colour, selectable rows, single-key verbs,
//! typed input through the same `Editing` prompt the sharing screen uses.
//! They exist only in a build with the `router` feature and only when the
//! dashboard was started with `--router`; otherwise the screens say so.

use super::*;
use kmplify_node::gpu::Gpu;
use kmplify_node::router::{self, cluster, JobState, Shared, Source};

/// What the dashboard holds for the router: the shared state its tasks
/// write, and the cursor on each screen.
pub(super) struct RouterState {
    pub shared: Shared,
    pub net_sel: usize,
    pub cluster_sel: usize,
    /// The address typed for a join, kept while the PIN is being typed.
    pub join_addr: String,
    pub joining: bool,
}

pub(super) const NETWORK_C: Color = Color::LightGreen;
pub(super) const CLUSTER_C: Color = Color::LightYellow;

/// Start the router's tasks in this process and return the state the
/// screens draw from. The same start the window does.
pub(super) fn start(dir: &std::path::Path, gpus: &[Gpu], accel: Backend) -> RouterState {
    let shared = router::new_shared(dir, gpus, router::lan_address());
    router::spawn(shared.clone(), accel);
    RouterState {
        shared,
        net_sel: 0,
        cluster_sel: 0,
        join_addr: String::new(),
        joining: false,
    }
}

fn off(f: &mut Frame, area: Rect, title: &str, colour: Color) {
    f.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  The LAN router is not running in this dashboard.",
                Style::new().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "  Start it with `kmplify-node tui --router` to find the other nodes on this",
                Style::new().fg(MUTED),
            )),
            Line::from(Span::styled(
                "  network, pair with them, and route requests across them. docs/ROUTER.md",
                Style::new().fg(MUTED),
            )),
        ])
        .block(panel_coloured(title, colour)),
        area,
    );
}

// ------------------------------------------------------------------ network

pub(super) fn draw_network(f: &mut Frame, app: &App, area: Rect) {
    let Some(rs) = &app.router else {
        off(f, area, "network", NETWORK_C);
        return;
    };
    let r = router::lock(&rs.shared).clone();
    let now = Instant::now();
    let [nodes_area, jobs_area, note] = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(8),
        Constraint::Length(3),
    ])
    .areas(area);

    let header = Line::from(Span::styled(
        format!(
            "  {:<14}{:<10}{:<16}{:<24}{:<24}{:>5}{:>5}{:>5}",
            "node", "state", "address", "gpu", "engines", "cpu", "gpu", "pend"
        ),
        Style::new().fg(MUTED),
    ));
    let mut lines = vec![header];
    for (i, n) in r.ordered().iter().enumerate() {
        let selected = i == rs.net_sel;
        let base = if selected {
            Style::new().fg(Color::Black).bg(NETWORK_C)
        } else {
            Style::new()
        };
        let online = n.online(now);
        let (state, colour) = if n.is_local() {
            ("LOCAL", Color::Green)
        } else if !online {
            ("OFFLINE", MUTED)
        } else if r.is_member(&n.id) {
            ("PAIRED", NETWORK_C)
        } else if !n.cluster_id.is_empty() {
            ("OTHER", Color::Yellow)
        } else {
            ("UNPAIRED", MUTED)
        };
        let engines: Vec<String> = n
            .running_engines()
            .map(|e| format!("{} {}", e.name, e.models.len()))
            .collect();
        let m = &n.metrics;
        let cpu = if m.sampled { format!("{:.0}%", m.cpu.latest()) } else { "-".into() };
        let gpu = if m.gpu_known { format!("{:.0}%", m.gpu.latest()) } else { "-".into() };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<14}", trunc(&n.name, 13)), base.add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<10}", state), if selected { base } else { base.fg(colour) }),
            Span::styled(format!("{:<16}", trunc(&n.address, 15)), base),
            Span::styled(
                format!("{:<24}", trunc(&n.gpus.first().map(|g| g.name.clone()).unwrap_or_else(|| "no gpu".into()), 23)),
                if selected { base } else { base.fg(MUTED) },
            ),
            Span::styled(
                format!("{:<24}", trunc(&if engines.is_empty() { "none answering".to_string() } else { engines.join(", ") }, 23)),
                base,
            ),
            Span::styled(format!("{:>5}{:>5}{:>5}", cpu, gpu, r.pending_for(&n.id)), base),
        ]));
    }
    if r.nodes.len() == 1 {
        lines.push(Line::from(Span::styled(
            "   only this machine so far — run the router on another computer on this network, or press a to add one by address",
            Style::new().fg(MUTED),
        )));
    }
    f.render_widget(
        Paragraph::new(lines).block(panel_coloured(
            &format!("network ({} node(s))", r.nodes.len()),
            NETWORK_C,
        )),
        nodes_area,
    );

    let mut job_lines: Vec<Line> = Vec::new();
    for j in r.jobs.iter().take(jobs_area.height.saturating_sub(2) as usize) {
        let (word, colour) = match j.state {
            JobState::Queued => ("queued", Color::Yellow),
            JobState::Running => ("running", Color::Green),
            JobState::Completed => ("done", MUTED),
            JobState::Failed => ("failed", Color::Red),
        };
        job_lines.push(Line::from(vec![
            Span::styled(format!("  {} ", status::clock_hms(j.at_ms)), Style::new().fg(MUTED)),
            Span::styled(format!("{:<8}", word), Style::new().fg(colour)),
            Span::styled(format!("{:<28}", trunc(if j.model.is_empty() { "(model unknown)" } else { &j.model }, 27)), Style::new().fg(Color::White)),
            Span::styled(format!("{} → {}", j.requested_from, j.ran_on), Style::new().fg(MUTED)),
            Span::styled(
                if j.error.is_empty() { String::new() } else { format!("  {}", j.error) },
                Style::new().fg(Color::Red),
            ),
        ]));
    }
    if job_lines.is_empty() {
        job_lines.push(Line::from(Span::styled(
            "  none yet — requests to the router's endpoints appear here, naming the node that served them",
            Style::new().fg(MUTED),
        )));
    }
    f.render_widget(
        Paragraph::new(job_lines).block(panel_coloured("routed jobs", NETWORK_C)),
        jobs_area,
    );

    let endpoints = format!(
        "endpoints  http://127.0.0.1:{} (Ollama API)  ·  http://127.0.0.1:{} (OpenAI API)  ·  discovery {}  ·  listeners {}",
        router::proxy_ollama_port(),
        router::proxy_openai_port(),
        r.discovery,
        r.listeners
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(endpoints, Style::new().fg(MUTED))))
            .wrap(Wrap { trim: true })
            .block(panel("router")),
        note,
    );
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Keys that only mean something on the network screen.
pub(super) fn on_network_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(rs) = app.router.as_mut() else {
        return false;
    };
    let count = router::lock(&rs.shared).nodes.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => rs.net_sel = rs.net_sel.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => rs.net_sel = (rs.net_sel + 1).min(count.saturating_sub(1)),
        KeyCode::Char('a') => {
            app.editing = Some(Editing {
                key: "add-node",
                label: "Address of a node on this network (host or host:port)",
                buffer: String::new(),
                masked: false,
            });
        }
        KeyCode::Esc => app.view = View::Home,
        _ => return false,
    }
    true
}

fn add_node(app: &mut App, address: String) {
    let Some(rs) = app.router.as_ref() else {
        return;
    };
    let address = address.trim().to_string();
    if address.is_empty() {
        return;
    }
    let mut r = router::lock(&rs.shared);
    if r.manual.iter().any(|a| a == &address) {
        drop(r);
        app.say(format!("{address} is already on the list"));
        return;
    }
    r.manual.push(address.clone());
    let (host, port) = router::Node::parse_address(&address);
    let mut node = router::Node::new_peer(
        format!("manual:{address}"),
        address.clone(),
        host,
        Source::Manual,
        Instant::now(),
    );
    node.info_port = port;
    node.last_seen = Instant::now() - router::PEER_TIMEOUT;
    r.upsert_peer(node);
    r.push_log(format!("added {address} by hand; probing its node-info surface"));
    drop(r);
    app.say(format!("added {address} — probing"));
}

// ------------------------------------------------------------------ cluster

pub(super) fn draw_cluster(f: &mut Frame, app: &App, area: Rect) {
    let Some(rs) = &app.router else {
        off(f, area, "cluster", CLUSTER_C);
        return;
    };
    let r = router::lock(&rs.shared).clone();
    let now = Instant::now();
    let [body, note] = Layout::vertical([Constraint::Min(6), Constraint::Length(5)]).areas(area);
    let mut lines: Vec<Line> = Vec::new();

    match &r.identity {
        Some(id) => lines.push(Line::from(vec![
            field("cert"),
            Span::styled(format!("{}…", cluster::short_fp(&id.fingerprint)), Style::new().fg(Color::White)),
            Span::styled("  this node's certificate fingerprint, what a peer pins", Style::new().fg(MUTED)),
        ])),
        None => lines.push(Line::from(Span::styled(
            " no certificate: this node cannot pair (see the log)",
            Style::new().fg(Color::Red),
        ))),
    }
    if r.cluster.is_clustered() {
        lines.push(Line::from(vec![
            field("cluster"),
            Span::styled(
                format!("{}…", &r.cluster.cluster_id[..8.min(r.cluster.cluster_id.len())]),
                Style::new().fg(Color::White),
            ),
            Span::styled(
                format!("  {} paired node(s)", r.cluster.members.len()),
                Style::new().fg(MUTED),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            field("cluster"),
            Span::styled("none — invite a node, or join one with the PIN it shows", Style::new().fg(MUTED)),
        ]));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        format!(" members ({})", r.cluster.members.len()),
        Style::new().fg(CLUSTER_C).add_modifier(Modifier::BOLD),
    )));
    for (i, m) in r.cluster.members.values().enumerate() {
        let selected = i == rs.cluster_sel;
        let base = if selected {
            Style::new().fg(Color::Black).bg(CLUSTER_C)
        } else {
            Style::new()
        };
        let online = r.nodes.get(&m.id).is_some_and(|n| n.online(now));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", if online { "●" } else { "○" }),
                if selected { base } else { base.fg(if online { Color::Green } else { MUTED }) },
            ),
            Span::styled(format!("{:<18}", trunc(&m.name, 17)), base.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{}…  {}…  ", &m.id[..8.min(m.id.len())], cluster::short_fp(&m.fingerprint)),
                if selected { base } else { base.fg(MUTED) },
            ),
            Span::styled(
                r.nodes.get(&m.id).map(|n| n.address.clone()).unwrap_or_default(),
                if selected { base } else { base.fg(MUTED) },
            ),
        ]));
    }
    if r.cluster.members.is_empty() {
        lines.push(Line::from(Span::styled("   none yet", Style::new().fg(MUTED))));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        " invitation",
        Style::new().fg(CLUSTER_C).add_modifier(Modifier::BOLD),
    )));
    match &r.invite {
        Some(inv) if !inv.expired() => {
            let addr = r
                .local()
                .map(|n| format!("{}:{}", n.address, router::node_info_port()))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled("   PIN ", Style::new().fg(MUTED)),
                Span::styled(inv.pin.clone(), Style::new().fg(CLUSTER_C).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("   enter it on the other machine with this address: {addr}"),
                    Style::new().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "   valid for {}, {} wrong attempt(s) so far   ·   n cancels it",
                    human(inv.remaining()),
                    inv.wrong_attempts
                ),
                Style::new().fg(MUTED),
            )));
        }
        _ => lines.push(Line::from(Span::styled(
            "   i  open an invitation: a PIN for five minutes, three wrong attempts",
            Style::new().fg(MUTED),
        ))),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " join",
        Style::new().fg(CLUSTER_C).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        if rs.joining {
            "   pairing…".to_string()
        } else {
            "   o  join a cluster: you are asked for the inviting node's address, then the PIN it shows".to_string()
        },
        Style::new().fg(if rs.joining { Color::Yellow } else { MUTED }),
    )));

    f.render_widget(
        Paragraph::new(lines).block(panel_coloured("cluster", CLUSTER_C)),
        body,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "i invite · o join · d remove the selected member · L leave the cluster · ↑/↓ select",
                Style::new().fg(MUTED),
            )),
            Line::from(Span::styled(
                "Requests between machines travel only between paired nodes, over mutual TLS pinned to these fingerprints. \
                 A six-digit PIN authenticates a SPAKE2 exchange, so it cannot be brute-forced from the network.",
                Style::new().fg(MUTED),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(panel("about")),
        note,
    );
}

/// Keys that only mean something on the cluster screen.
pub(super) fn on_cluster_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(rs) = app.router.as_mut() else {
        return false;
    };
    let members: Vec<(String, String)> = router::lock(&rs.shared)
        .cluster
        .members
        .values()
        .map(|m| (m.id.clone(), m.name.clone()))
        .collect();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => rs.cluster_sel = rs.cluster_sel.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            rs.cluster_sel = (rs.cluster_sel + 1).min(members.len().saturating_sub(1))
        }
        KeyCode::Char('i') => {
            let pin = router::lock(&rs.shared).open_invite();
            app.say(format!("invitation open: PIN {pin}"));
        }
        KeyCode::Char('n') => {
            router::lock(&rs.shared).invite = None;
            app.say("invitation cancelled");
        }
        KeyCode::Char('o') => {
            if rs.joining {
                app.say("a join is already in progress");
            } else {
                app.editing = Some(Editing {
                    key: "join-addr",
                    label: "Address of the inviting node (host or host:port)",
                    buffer: String::new(),
                    masked: false,
                });
            }
        }
        KeyCode::Char('d') => match members.get(rs.cluster_sel) {
            Some((id, name)) => app.confirm = Some(Confirm::RemoveMember(id.clone(), name.clone())),
            None => app.say("select a member first"),
        },
        KeyCode::Char('L') => app.confirm = Some(Confirm::Leave),
        KeyCode::Esc => app.view = View::Home,
        _ => return false,
    }
    true
}

pub(super) fn remove_member(app: &mut App, id: &str) {
    if let Some(rs) = app.router.as_ref() {
        let mut r = router::lock(&rs.shared);
        r.remove_member(id);
        r.push_log(format!("removed {} from the cluster", &id[..8.min(id.len())]));
        drop(r);
        app.say("member removed and unpinned");
    }
}

pub(super) fn leave(app: &mut App) {
    if let Some(rs) = app.router.as_ref() {
        let mut r = router::lock(&rs.shared);
        r.leave_cluster();
        r.push_log("left the cluster; every pin dropped");
        drop(r);
        app.say("left the cluster");
    }
}

/// A finished text prompt that belongs to these screens. `true` when it
/// was one of ours.
pub(super) fn on_edit(app: &mut App, key: &str, value: String) -> bool {
    match key {
        "add-node" => {
            add_node(app, value);
            true
        }
        "join-addr" => {
            if let Some(rs) = app.router.as_mut() {
                rs.join_addr = value.trim().to_string();
            }
            app.editing = Some(Editing {
                key: "join-pin",
                label: "The six-digit PIN shown on the inviting node",
                buffer: String::new(),
                masked: true,
            });
            true
        }
        "join-pin" => {
            let Some(rs) = app.router.as_mut() else {
                return true;
            };
            let (shared, addr) = (rs.shared.clone(), rs.join_addr.clone());
            rs.joining = true;
            let tx = app.peer_tx.clone();
            tokio::spawn(async move {
                let out = cluster::join(shared, addr, value).await;
                let _ = tx.send(BgMsg::Router(out));
            });
            app.say("pairing…");
            true
        }
        _ => false,
    }
}

pub(super) fn on_router_msg(app: &mut App, result: Result<String, String>) {
    if let Some(rs) = app.router.as_mut() {
        rs.joining = false;
    }
    match result {
        Ok(m) => app.say(m),
        Err(e) => app.say(format!("pairing failed: {e}")),
    }
}

/// Rendered into ratatui's test backend: what the screens say, without a
/// terminal. The window is verified by looking; these screens are
/// verified by reading, which a headless node's operator does too.
#[cfg(test)]
mod tests {
    use super::*;
    use kmplify_node::router::cluster::{Invite, Member};
    use kmplify_node::router::{Engine, Node, Router};
    use ratatui::backend::TestBackend;
    use std::sync::{Arc, Mutex};

    /// This machine (King) paired with one peer (Spark) that serves two
    /// models, and an invitation open.
    fn shared() -> Shared {
        let now = Instant::now();
        let mut r = Router {
            self_id: "me00000000".into(),
            lan_ingress: true,
            node_dir: std::env::temp_dir().join("kmplify-tui-router-test"),
            ..Default::default()
        };
        let mut me = Node::new_peer("me00000000".into(), "King".into(), "192.168.1.2".into(), Source::Local, now);
        me.gpus.push(kmplify_node::router::GpuInfo { name: "RTX 4090".into(), total_mb: 24000 });
        me.metrics.cpu.push(12.0);
        me.metrics.sampled = true;
        r.nodes.insert(me.id.clone(), me);
        let mut peer = Node::new_peer("peer000000".into(), "Spark".into(), "192.168.1.25".into(), Source::Member, now);
        peer.engines.push(Engine {
            id: "ollama".into(),
            name: "Ollama".into(),
            base: "http://127.0.0.1:11434".into(),
            models: vec!["qwen3:0.6b".into(), "gemma4:latest".into()],
            running: true,
            installed: true,
            owned: false,
        });
        peer.metrics.observe_gpu(40.0, now);
        r.nodes.insert(peer.id.clone(), peer);
        r.cluster.cluster_id = "c1c1c1c1c1".into();
        r.cluster.members.insert(
            "peer000000".into(),
            Member {
                id: "peer000000".into(),
                name: "Spark".into(),
                fingerprint: "abcdef0123456789abcdef".into(),
                added_ms: 0,
                address: "192.168.1.25".into(),
                info_port: 14418,
            },
        );
        r.invite = Some(Invite::new());
        r.discovery = "advertising and browsing".into();
        r.listeners = "listening".into();
        Arc::new(Mutex::new(r))
    }

    fn app_with_router() -> App {
        let mut app = crate::tui::tests::app();
        app.router = Some(RouterState {
            shared: shared(),
            net_sel: 0,
            cluster_sel: 0,
            join_addr: String::new(),
            joining: false,
        });
        app
    }

    fn render(app: &App, draw: fn(&mut Frame, &App, Rect)) -> String {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(150, 32)).unwrap();
        terminal.draw(|f| draw(f, app, f.area())).unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_network_screen_lists_every_node_with_its_state_and_engines() {
        let app = app_with_router();
        let text = render(&app, draw_network);
        assert!(text.contains("King"), "{text}");
        assert!(text.contains("LOCAL"), "{text}");
        assert!(text.contains("Spark"), "{text}");
        assert!(text.contains("PAIRED"), "{text}");
        assert!(text.contains("Ollama 2"), "the peer's engine and model count: {text}");
        assert!(text.contains("RTX 4090"), "{text}");
        assert!(text.contains("advertising and browsing"), "{text}");
        assert!(text.contains("routed jobs"), "{text}");
    }

    #[test]
    fn the_cluster_screen_shows_the_pin_the_members_and_the_verbs() {
        let app = app_with_router();
        let pin = router::lock(&app.router.as_ref().unwrap().shared).invite.as_ref().unwrap().pin.clone();
        let text = render(&app, draw_cluster);
        assert!(text.contains(&pin), "the PIN is on screen: {text}");
        assert!(text.contains("192.168.1.2:14418"), "with this machine's address: {text}");
        assert!(text.contains("Spark"), "{text}");
        assert!(text.contains("abcdef0123456789"), "the member's fingerprint, shortened: {text}");
        assert!(text.contains("1 paired node"), "{text}");
        assert!(text.contains("i invite"), "{text}");
    }

    #[test]
    fn without_the_router_both_screens_say_how_to_start_it() {
        let app = crate::tui::tests::app();
        for draw in [draw_network as fn(&mut Frame, &App, Rect), draw_cluster] {
            let text = render(&app, draw);
            assert!(text.contains("--router"), "{text}");
        }
    }

    #[test]
    fn keys_8_and_9_open_the_screens_and_the_verbs_prompt_for_input() {
        let mut app = app_with_router();
        app.on_key(KeyEvent::new(KeyCode::Char('8'), KeyModifiers::NONE));
        assert_eq!(app.view, View::Network);
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.editing.as_ref().map(|e| e.key), Some("add-node"));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.editing.is_none(), "esc cancels the prompt");

        app.on_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
        assert_eq!(app.view, View::Cluster);
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(app.editing.as_ref().map(|e| e.key), Some("join-addr"));
        for c in "10.0.0.5:24418".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.router.as_ref().unwrap().join_addr, "10.0.0.5:24418");
        let pin_prompt = app.editing.as_ref().unwrap();
        assert_eq!(pin_prompt.key, "join-pin");
        assert!(pin_prompt.masked, "a PIN is never echoed");
    }

    #[test]
    fn removing_and_leaving_ask_first_then_act() {
        let mut app = app_with_router();
        app.view = View::Cluster;
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(app.confirm, Some(Confirm::RemoveMember(ref id, _)) if id == "peer000000"));
        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(!router::lock(&app.router.as_ref().unwrap().shared).is_member("peer000000"));

        app.on_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE));
        assert!(matches!(app.confirm, Some(Confirm::Leave)));
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.confirm.is_none(), "anything but y cancels");
        assert!(router::lock(&app.router.as_ref().unwrap().shared).cluster.is_clustered());
    }

    #[test]
    fn a_node_added_by_address_gets_a_card_and_is_probed_at_that_port() {
        let mut app = app_with_router();
        add_node(&mut app, "10.0.0.9:24418".into());
        let r = router::lock(&app.router.as_ref().unwrap().shared);
        let n = &r.nodes["manual:10.0.0.9:24418"];
        assert_eq!(n.address, "10.0.0.9");
        assert_eq!(n.info_port, 24418);
        assert_eq!(r.manual, vec!["10.0.0.9:24418"]);
    }
}
