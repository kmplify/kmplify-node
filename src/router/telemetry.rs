//! Sampling this machine for its own card, and keeping the directory honest.
//!
//! PAIR's node-info service does this in a process of its own and serves it
//! over HTTP; here the same numbers come from the crate's existing probes
//! (`hostcpu` for CPU and RAM, `gpu::utilization` for the card) and land
//! straight in the shared state. The HTTP surface that lets *peers* read
//! them is phase 2 (docs/ROUTER.md); until then a peer's card carries the
//! static facts its announcement brought and says its live meters are not
//! available yet, rather than charting zeros.

use std::time::{Duration, Instant};

use crate::gpu::Backend;
use crate::{engines, gpu, hostcpu};

use super::{lock, Engine, Shared};

/// CPU and RAM every second, as the desktop app samples; the GPU probe is a
/// subprocess (nvidia-smi and friends), so it runs every other tick — a node
/// lending its cycles should not spend them on being watched.
const TICK: Duration = Duration::from_secs(1);
const GPU_EVERY: u32 = 2;

pub async fn sample_local(shared: Shared, accel: Backend) {
    hostcpu::start();
    let mut tick: u32 = 0;
    let mut gpu_pct: Option<u8> = None;
    let mut vram_used: Option<u64> = None;
    loop {
        tokio::time::sleep(TICK).await;
        tick = tick.wrapping_add(1);
        let cpu = hostcpu::snapshot();
        if accel != Backend::Cpu && tick % GPU_EVERY == 0 {
            let (p, v) = gpu::utilization(accel).await;
            gpu_pct = p;
            vram_used = v;
        }
        let mut r = lock(&shared);
        let Some(me) = r.local_mut() else {
            continue;
        };
        let vram_total = me.gpus.first().map(|g| g.total_mb).unwrap_or(0);
        let m = &mut me.metrics;
        // The sampler needs two refreshes before its first real percentage;
        // publishing that first 0 would open every session with a dip.
        if cpu.sampled {
            m.cpu.push(cpu.percent);
            m.sampled = true;
        }
        m.ram_used_mb = cpu.ram_used_mb;
        m.ram.push(percent(cpu.ram_used_mb, cpu.ram_total_mb.max(me.ram_total_mb)));
        if let Some(p) = gpu_pct {
            m.gpu.push(p as f32);
            m.gpu_known = true;
        }
        // Unified memory has no distinct "used VRAM"; the same rule as the
        // dashboard's meters, so the two never disagree about a Mac.
        if matches!(accel, Backend::Cuda | Backend::Rocm) {
            if let Some(used) = vram_used {
                m.vram_used_mb = used;
                m.vram.push(percent(used, vram_total));
                m.vram_known = vram_total > 0;
            }
        }
        me.last_seen = Instant::now();
    }
}

fn percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// Which engines answer on this machine, and what they serve. The whole
/// roster is listed, running or not, because the card is also where an
/// operator learns what this machine could run.
const ENGINE_SCAN: Duration = Duration::from_secs(10);

pub async fn scan_engines(shared: Shared) {
    loop {
        let found = engines::scan().await;
        let list = roster_from(&found);
        {
            let mut r = lock(&shared);
            let mut changed = false;
            if let Some(me) = r.local_mut() {
                changed = me.engines != list;
                me.engines = list;
            }
            if changed {
                let summary: Vec<String> = r
                    .local()
                    .map(|n| {
                        n.running_engines()
                            .map(|e| format!("{} ({} models)", e.name, e.models.len()))
                            .collect()
                    })
                    .unwrap_or_default();
                r.push_log(if summary.is_empty() {
                    "no inference engine is answering on this machine".to_string()
                } else {
                    format!("engines: {}", summary.join(", "))
                });
            }
        }
        tokio::time::sleep(ENGINE_SCAN).await;
    }
}

/// The known roster with the live scan folded in. Order follows
/// `engines::KNOWN`, so the badges never reshuffle between scans; an engine
/// the scan identified but the roster does not know (a bare OpenAI-compatible
/// server) is appended.
pub fn roster_from(found: &[engines::Found]) -> Vec<Engine> {
    let mut out: Vec<Engine> = engines::KNOWN
        .iter()
        .map(|k| {
            let live = found.iter().find(|f| f.id == k.id);
            Engine {
                id: k.id.to_string(),
                name: k.name.to_string(),
                base: live
                    .map(|f| f.base.clone())
                    .unwrap_or_else(|| k.default_base.to_string()),
                models: live.map(|f| f.models.clone()).unwrap_or_default(),
                running: live.is_some(),
            }
        })
        .collect();
    for f in found {
        if !out.iter().any(|e| e.id == f.id) {
            out.push(Engine {
                id: f.id.clone(),
                name: f.name.clone(),
                base: f.base.clone(),
                models: f.models.clone(),
                running: true,
            });
        }
    }
    out
}

/// Drop peers that stopped announcing. Discovery removals arrive too, but
/// multicast is lossy in both directions, so a timer is the backstop.
pub async fn expire_peers(shared: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        lock(&shared).expire(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_roster_lists_every_known_engine_and_marks_the_live_ones() {
        let found = vec![engines::Found {
            base: "http://127.0.0.1:1234".into(),
            id: "lmstudio".into(),
            name: "LM Studio".into(),
            models: vec!["a".into(), "b".into()],
        }];
        let roster = roster_from(&found);
        assert_eq!(roster.len(), engines::KNOWN.len());
        let lm = roster.iter().find(|e| e.id == "lmstudio").unwrap();
        assert!(lm.running);
        assert_eq!(lm.models.len(), 2);
        let ollama = roster.iter().find(|e| e.id == "ollama").unwrap();
        assert!(!ollama.running);
        assert!(ollama.models.is_empty());
        assert_eq!(ollama.base, "http://127.0.0.1:11434");
    }

    #[test]
    fn an_unknown_openai_server_is_appended_not_lost() {
        let found = vec![engines::Found {
            base: "http://127.0.0.1:9999".into(),
            id: "openai-compatible".into(),
            name: "OpenAI-compatible".into(),
            models: vec![],
        }];
        let roster = roster_from(&found);
        assert_eq!(roster.len(), engines::KNOWN.len() + 1);
        assert!(roster.last().unwrap().running);
    }

    #[test]
    fn percent_never_divides_by_zero_or_exceeds_100() {
        assert_eq!(percent(5, 0), 0.0);
        assert_eq!(percent(50, 100), 50.0);
        assert_eq!(percent(500, 100), 100.0);
    }
}
