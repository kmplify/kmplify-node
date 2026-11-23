//! The startup summary and `kmplify-node status`: what is connected, what is
//! shared, where the node stands on the way to Trusted, and what to do next.
//!
//! Written for the person at the machine, not for a log parser: whole
//! sentences, no internal state names, and an informational line never
//! worded like an error. Pure text from plain inputs, so every wording is
//! tested.

use crate::security::Report;
use crate::status::Snapshot;
use crate::trust::{self, FabricView};

/// Everything one summary needs.
pub struct Facts<'a> {
    pub snap: &'a Snapshot,
    pub report: Option<&'a Report>,
    pub fabric: Option<&'a FabricView>,
    /// Where a running security check is.
    pub progress: &'a crate::security::Progress,
    /// The engine the node lends (`OLLAMA_BASE`), or "" when not known here.
    pub engine: &'a str,
    /// The local dashboard's URL, when it runs.
    pub dashboard_url: Option<&'a str>,
    pub now_s: u64,
    /// ANSI colour for a terminal.
    pub color: bool,
}

/// How many missing items the summary lists before "and N more".
pub const TOP_MISSING: usize = 3;

fn paint(color: bool, code: &str, s: &str) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Does this node offer anything besides chat models?
pub fn other_lanes(s: &Snapshot) -> bool {
    !s.workloads.is_empty() || s.share_cpu || s.functions_enabled || s.vectors_enabled
}

/// The chat-model line, and what "no models" means, in plain words.
pub fn models_text(s: &Snapshot, engine: &str) -> String {
    if !s.share_inference {
        return "off (sharing chat models is switched off)".into();
    }
    if s.paused {
        return "paused by you: nothing is advertised until you resume".into();
    }
    if !s.models.is_empty() {
        let shown: Vec<&str> = s.models.iter().take(4).map(String::as_str).collect();
        let more = s.models.len().saturating_sub(shown.len());
        return format!(
            "{} shared ({}{})",
            s.models.len(),
            shown.join(", "),
            if more > 0 {
                format!(", and {more} more")
            } else {
                String::new()
            }
        );
    }
    let at = if engine.is_empty() {
        String::new()
    } else {
        format!(" (none found at {engine})")
    };
    if other_lanes(s) {
        format!(
            "none{at}. No language models shared for chat; serverless apps are still offered. \
             This is information, not an error: start your model engine to share chat models too."
        )
    } else {
        format!(
            "none{at}. No language models shared for chat, and nothing else is shared: the node \
             is connected but has nothing to serve until your engine lists a model."
        )
    }
}

fn onoff(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

/// The readiness meter in a terminal: the bar with its verdict, then the
/// three conditions for Trusted as steps. The same words as the dashboard.
pub fn readiness_lines(
    report: Option<&Report>,
    fabric: Option<&FabricView>,
    progress: &crate::security::Progress,
    now_s: u64,
    color: bool,
) -> Vec<String> {
    let r = trust::readiness(report, fabric, progress, now_s);
    let code = match r.band {
        "green" => "32",
        "amber" => "33",
        "red" => "31",
        _ => "2",
    };
    let mut out = vec![format!(
        "  Readiness   {}  {}",
        paint(color, code, &trust::readiness_bar(&r)),
        r.verdict
    )];
    for (n, step) in r.steps.iter().enumerate() {
        let mark = match step.state {
            "done" => paint(color, "32", "[done]"),
            "missing" => paint(color, "31", "[todo]"),
            "checking" => paint(color, "2", "[ .. ]"),
            _ => paint(color, "33", "[open]"),
        };
        out.push(format!(
            "    Step {}  {mark} {}: {}",
            n + 1,
            step.label,
            step.text
        ));
    }
    out
}

/// The trust part, shared by the startup summary and `status`.
pub fn trust_lines(
    report: Option<&Report>,
    fabric: Option<&FabricView>,
    progress: &crate::security::Progress,
    now_s: u64,
    color: bool,
) -> Vec<String> {
    let st = trust::standing(report, fabric, now_s);
    let level = match st.level.as_str() {
        "trusted" => paint(color, "32", "TRUSTED"),
        "cloud" => paint(color, "32", "CLOUD"),
        "community" => paint(color, "33", "COMMUNITY"),
        _ => paint(color, "2", "NOT KNOWN YET"),
    };
    let mut out = vec![format!("  Trust       {level}: {}", st.headline)];
    if let Some(f) = fabric {
        if f.state != trust::Match::Matched && f.state != trust::Match::Pending {
            out.push(format!("              fabric: {}", trust::fabric_line(f)));
        }
    }
    out.extend(readiness_lines(report, fabric, progress, now_s, color));
    let mut bad = Vec::new();
    if let Some(r) = report {
        for item in trust::checklist(r, fabric) {
            if item.state == "fail" || item.state == "unknown" {
                bad.push(item);
            }
        }
    }
    let labels: Vec<&str> = crate::security::Item::ALL
        .iter()
        .map(|i| i.label())
        .collect();
    let extra: Vec<&String> = st
        .missing
        .iter()
        .filter(|m| !labels.iter().any(|l| m.starts_with(l)))
        .collect();
    if bad.is_empty() && extra.is_empty() {
        if st.baseline_ok == Some(true) {
            out.push("              Security baseline: every item passes on this machine.".into());
        }
        return out;
    }
    out.push("    Missing for a Trusted node:".into());
    for item in bad.iter().take(TOP_MISSING) {
        let mark = if item.state == "fail" {
            paint(color, "31", "[FAIL]")
        } else {
            paint(color, "31", "[ ?? ]")
        };
        out.push(format!("      {mark} {}: {}", item.label, item.text));
        if let Some(first) = item.steps.first() {
            out.push(format!("             Fix: {first}"));
        }
    }
    if bad.len() > TOP_MISSING {
        out.push(format!(
            "      and {} more: see `kmplify-node security check`",
            bad.len() - TOP_MISSING
        ));
    }
    for m in extra {
        out.push(format!("      - {m}"));
    }
    if let Some(r) = report {
        if r.elevated == Some(false) && r.disk_encrypted.is_none() && r.os == "windows" {
            out.push(
                "      Note: this node runs without administrator rights, so Windows does not \
                 show it the BitLocker status. See `kmplify-node security check`."
                    .into(),
            );
        }
    }
    out
}

/// Every baseline item in one line each, with the fabric's verdict when it
/// is known: the checklist part of `kmplify-node status`.
pub fn checklist_lines(report: &Report, fabric: Option<&FabricView>, color: bool) -> Vec<String> {
    let mut out = vec![format!(
        "  Checklist   this machine ({}), checked {}",
        report.os,
        ago(report.checked_at)
    )];
    for item in trust::checklist(report, fabric) {
        let mark = match item.state {
            "pass" => paint(color, "32", "[ ok ]"),
            "fail" => paint(color, "31", "[FAIL]"),
            "unknown" => paint(color, "31", "[ ?? ]"),
            _ => paint(color, "2", "[ -- ]"),
        };
        let fab = match item.fabric {
            Some("pass") => "  | fabric: passes",
            Some(_) => "  | fabric: fails",
            None => "",
        };
        out.push(format!("    {mark} {:<20} {}{fab}", item.label, item.text));
    }
    out
}

fn ago(unix_s: u64) -> String {
    let now = crate::identity::now_s();
    let d = now.saturating_sub(unix_s);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        _ => format!("{} h {} min ago", d / 3600, d % 3600 / 60),
    }
}

/// The block the node prints once it is up.
pub fn render(f: &Facts) -> String {
    let s = f.snap;
    let rule = "-".repeat(72);
    let mut out = vec![rule.clone()];
    let short = &s.node_id[..8.min(s.node_id.len())];
    out.push(format!(
        "kmplify-node {} {}",
        s.version,
        if short.is_empty() {
            "(no node identity yet)".to_string()
        } else {
            format!("(node {short})")
        }
    ));
    let link = match s.link {
        crate::status::Link::Online => paint(f.color, "32", "connected"),
        _ => paint(f.color, "33", &s.link.label().to_ascii_lowercase()),
    };
    out.push(format!("  Fabric      {link} to {}", s.gateway));
    if s.link != crate::status::Link::Online && !s.link_detail.is_empty() {
        out.push(format!("              last attempt: {}", s.link_detail));
    }
    let mut machine = Vec::new();
    if !s.gpu_name.is_empty() {
        machine.push(format!(
            "{} {} GB ({})",
            s.gpu_name,
            s.vram_total_mb / 1024,
            s.accelerator
        ));
    } else if !s.accelerator.is_empty() {
        machine.push(format!("accelerator {}", s.accelerator));
    }
    if s.cpus > 0.0 {
        machine.push(format!("{} CPU threads", s.cpus));
    }
    if s.ram_total_mb > 0 {
        machine.push(format!("{} GB RAM", s.ram_total_mb / 1024));
    }
    if !machine.is_empty() {
        out.push(format!("  Machine     {}", machine.join(" | ")));
    }
    out.push("  Sharing".into());
    out.push(format!("    Chat models   {}", models_text(s, f.engine)));
    out.push(format!(
        "    Serverless    {}",
        if s.workloads.is_empty() {
            "off (no app templates offered)".to_string()
        } else {
            format!(
                "{} (apps run as container sessions)",
                s.workloads.join(", ")
            )
        }
    ));
    out.push(format!("    CPU and RAM   {}", onoff(s.share_cpu)));
    out.push(format!("    Functions     {}", onoff(s.functions_enabled)));
    out.push(format!("    Vectors       {}", onoff(s.vectors_enabled)));
    out.extend(trust_lines(
        f.report, f.fabric, f.progress, f.now_s, f.color,
    ));
    out.push("  Details     `kmplify-node security check`  every item with how to fix it".into());
    out.push("              `kmplify-node status`          this summary again".into());
    if let Some(url) = f.dashboard_url {
        out.push(format!(
            "  Dashboard   {url}  (opens only on this computer)"
        ));
    }
    out.push(rule);
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::Progress;
    use crate::trust::Match;

    fn snap() -> Snapshot {
        Snapshot {
            version: "0.8.0".into(),
            node_id: "8f2c1a5b90de4a17b3c9d0e5f6a7b8c9".into(),
            gateway: "https://fabric.example".into(),
            link: crate::status::Link::Online,
            share_inference: true,
            workloads: vec!["comfyui".into()],
            cpus: 16.0,
            ram_total_mb: 65536,
            ..Snapshot::default()
        }
    }

    fn report() -> Report {
        Report {
            os: "windows".into(),
            disk_encrypted: None,
            patch_age_days: Some(3),
            pending_security_updates: Some(0),
            firewall_on: Some(true),
            screen_lock: Some(false),
            checked_at: 1_800_000_000,
            checker_version: "2".into(),
            elevated: Some(false),
            ..Report::default()
        }
    }

    #[test]
    fn no_models_reads_as_information_when_apps_are_offered() {
        let s = snap();
        let t = models_text(&s, "http://127.0.0.1:11434");
        assert!(
            t.contains("No language models shared for chat; serverless apps are still offered"),
            "{t}"
        );
        assert!(!t.contains("refused"), "{t}");
        let alone = Snapshot {
            workloads: vec![],
            ..snap()
        };
        assert!(models_text(&alone, "").contains("nothing else is shared"));
        let off = Snapshot {
            share_inference: false,
            ..snap()
        };
        assert!(models_text(&off, "").starts_with("off"));
    }

    #[test]
    fn the_summary_says_what_is_connected_shared_and_missing() {
        let fabric = FabricView {
            state: Match::Matched,
            trust_level: Some("community".into()),
            baseline_ok: Some(false),
            failing: vec!["disk_encrypted".into(), "screen_lock".into()],
            ..FabricView::default()
        };
        let r = report();
        let text = render(&Facts {
            snap: &snap(),
            report: Some(&r),
            fabric: Some(&fabric),
            progress: &Progress::default(),
            engine: "http://127.0.0.1:11434",
            dashboard_url: Some("http://127.0.0.1:5000/?t=x"),
            now_s: 1_800_000_000,
            color: false,
        });
        for want in [
            "(node 8f2c1a5b)",
            "connected to https://fabric.example",
            "Serverless    comfyui",
            "COMMUNITY",
            "[ ?? ] Disk encryption",
            "Fix: Turn on BitLocker",
            "[FAIL] Screen lock: off",
            "Readiness   [-##-##] 4/6  Ready for Trusted: 4 of 6 checks passed; missing: disk \
             encryption, screen lock.",
            "Step 1  [todo] Security baseline (this machine): 4 of 6 passed",
            "Step 2  [open] Identity check (provider account): your KMPLIFY provider account",
            "Step 3  [open] Trust grant (KMPLIFY): KMPLIFY grants Trusted",
            "without administrator rights",
            "kmplify-node security check",
            "Dashboard   http://127.0.0.1:5000/?t=x",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains('\u{2014}'), "no em dash in operator text");
        assert!(!text.contains("\x1b["), "no colour when asked for none");
    }

    #[test]
    fn many_missing_items_are_cut_to_the_top_ones() {
        let r = Report {
            firewall_on: Some(false),
            patch_age_days: Some(90),
            pending_security_updates: Some(4),
            ..report()
        };
        let lines =
            trust_lines(Some(&r), None, &Progress::default(), 1_800_000_000, false).join("\n");
        assert!(lines.contains("and 1 more"), "{lines}");
        assert!(lines.contains("NOT KNOWN YET"), "{lines}");
    }

    #[test]
    fn the_status_checklist_lists_every_item_with_the_fabrics_verdict() {
        let fabric = FabricView {
            state: Match::Matched,
            trust_level: Some("community".into()),
            baseline_ok: Some(false),
            failing: vec!["screen_lock".into()],
            ..FabricView::default()
        };
        let lines = checklist_lines(&report(), Some(&fabric), false);
        assert_eq!(lines.len(), 1 + crate::security::Item::ALL.len());
        let text = lines.join("\n");
        assert!(text.contains("[FAIL] Screen lock"), "{text}");
        assert!(text.contains("fabric: fails"), "{text}");
        assert!(text.contains("[ ok ] Firewall"), "{text}");
        assert!(text.contains("[ -- ] SSH root login"), "{text}");
        assert!(!checklist_lines(&report(), None, false)
            .join("\n")
            .contains("fabric:"));
    }

    #[test]
    fn a_passing_machine_says_so() {
        let r = Report {
            disk_encrypted: Some(true),
            screen_lock: Some(true),
            ..report()
        };
        let trusted = FabricView {
            state: Match::Matched,
            trust_level: Some("trusted".into()),
            site: Some("office".into()),
            baseline_ok: Some(true),
            ..FabricView::default()
        };
        let lines = trust_lines(
            Some(&r),
            Some(&trusted),
            &Progress::default(),
            1_800_000_000,
            false,
        )
        .join("\n");
        assert!(lines.contains("TRUSTED"), "{lines}");
        assert!(lines.contains("every item passes"), "{lines}");
        assert!(!lines.contains("Missing"), "{lines}");
    }
}
