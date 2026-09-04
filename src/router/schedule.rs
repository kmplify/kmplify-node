//! Which node should take the next request.
//!
//! PAIR's policy, kept deliberately coarse: a node's rank is the work it is
//! already holding plus how hard its busiest GPU is working, mapped to a
//! handful of pressure units with hysteresis so the order does not flap
//! around a threshold. It is load feedback, not a capacity model — it does
//! not know a 4090 from a laptop chip — and docs/ROUTER.md lists what that
//! misses. Capability (does the node have the model at all) is decided
//! before this runs; the scheduler is model-blind on purpose.

use std::time::Instant;

use super::{Router, TELEMETRY_STALE};

/// Exponential smoothing of the GPU figure. One busy second should not
/// reorder the cluster; a sustained load should within a few samples.
pub fn smooth(prev: f32, sample: f32) -> f32 {
    prev * 0.6 + sample * 0.4
}

/// Pressure units from the smoothed utilisation: 40 %, 70 % and 85 % on
/// the way up, ten points lower on the way down, one step per sample. The
/// gap is the hysteresis; a node hovering at 70 % is not re-ranked twice a
/// second.
pub fn pressure_step(prev: u8, smoothed: f32) -> u8 {
    let up = if smoothed >= 85.0 {
        3
    } else if smoothed >= 70.0 {
        2
    } else if smoothed >= 40.0 {
        1
    } else {
        0
    };
    if up >= prev {
        return up;
    }
    let floor = match prev {
        3 => 75.0,
        2 => 60.0,
        1 => 30.0,
        _ => 0.0,
    };
    if smoothed < floor {
        prev - 1
    } else {
        prev
    }
}

/// The pressure a node contributes: its own figure while the GPU sample is
/// fresh, otherwise a neutral 1. Missing telemetry must not read as idle,
/// or every request would pile onto the node nobody can see.
pub fn pressure_of(router: &Router, node_id: &str, now: Instant) -> u8 {
    let Some(n) = router.nodes.get(node_id) else {
        return 1;
    };
    match n.metrics.gpu_sampled_at {
        Some(at) if now.duration_since(at) < TELEMETRY_STALE => n.metrics.pressure,
        _ => 1,
    }
}

/// Order candidates best first: pending plus pressure, then pressure, then
/// id — so a cold start is deterministic rather than random.
pub fn rank(router: &Router, candidates: &[String], now: Instant) -> Vec<String> {
    let mut scored: Vec<(u32, u8, String)> = candidates
        .iter()
        .map(|id| {
            let pending = router.pending_for(id);
            let pressure = pressure_of(router, id, now);
            (pending + pressure as u32, pressure, id.clone())
        })
        .collect();
    scored.sort();
    scored.into_iter().map(|(_, _, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{Node, Source};

    #[test]
    fn pressure_rises_at_the_thresholds_and_falls_ten_points_lower() {
        assert_eq!(pressure_step(0, 39.0), 0);
        assert_eq!(pressure_step(0, 40.0), 1);
        assert_eq!(pressure_step(1, 70.0), 2);
        assert_eq!(pressure_step(2, 85.0), 3);
        assert_eq!(pressure_step(3, 80.0), 3, "still above the down threshold");
        assert_eq!(pressure_step(3, 74.0), 2);
        assert_eq!(pressure_step(2, 59.0), 1);
        assert_eq!(pressure_step(1, 35.0), 1);
        assert_eq!(pressure_step(1, 29.0), 0);
        assert_eq!(pressure_step(0, 90.0), 3, "a jump up is immediate");
    }

    #[test]
    fn stale_or_missing_telemetry_is_neutral_not_idle() {
        let now = Instant::now();
        let mut r = Router::default();
        let mut fresh = Node::new_peer("f".into(), "f".into(), String::new(), Source::Discovered, now);
        fresh.metrics.observe_gpu(95.0, now);
        let mut stale = Node::new_peer("s".into(), "s".into(), String::new(), Source::Discovered, now);
        stale.metrics.observe_gpu(0.0, now - TELEMETRY_STALE - std::time::Duration::from_secs(1));
        r.nodes.insert("f".into(), fresh);
        r.nodes.insert("s".into(), stale);
        assert_eq!(pressure_of(&r, "f", now), 3);
        assert_eq!(pressure_of(&r, "s", now), 1);
        assert_eq!(pressure_of(&r, "unknown", now), 1);
    }

    #[test]
    fn rank_prefers_idle_nodes_then_ties_on_id() {
        let now = Instant::now();
        let mut r = Router::default();
        for (id, pending, gpu) in [("c", 0, 0.0), ("a", 0, 0.0), ("b", 2, 0.0), ("d", 0, 90.0)] {
            let mut n = Node::new_peer(id.into(), id.into(), String::new(), Source::Discovered, now);
            n.reported_pending = pending;
            n.metrics.observe_gpu(gpu, now);
            r.nodes.insert(id.into(), n);
        }
        let order = rank(&r, &["a".into(), "b".into(), "c".into(), "d".into()], now);
        assert_eq!(order, ["a", "c", "b", "d"]);
    }
}
