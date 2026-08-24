//! Vector collections on this node: the RAG-on-peers lane (protocol v3.0).
//!
//! A consumer's index (document embeddings plus an opaque payload per
//! point) is replicated onto peers that lend vector storage. This module
//! is the replica: it stores points for collections the gateway assigned
//! here, answers nearest-neighbour queries, and enforces the owner's disk
//! ceiling.
//!
//! Honest scope. This is a correct, bounded, brute-force store, not an
//! approximate-nearest-neighbour engine: every query scans the collection
//! in memory. That is the right first implementation for the sizes a
//! personal RAG index has (thousands to low millions of dimensions' worth
//! of floats), and it keeps the on-disk format trivially auditable. An ANN
//! index is a later optimisation behind the same job frames.
//!
//! Privacy posture: payloads are opaque bytes. The node never interprets
//! them and has no key to. The vectors themselves are the consumer's data
//! too, which is why this lane is opt-in on both sides (`PROVIDER_SHARE_VECTORS`
//! here, the consumer's own security setting there).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Hard ceiling a collection's in-memory footprint may reach on this node,
/// whatever the operator configures; keeps a runaway consumer from
/// exhausting RAM before the disk cap is even reached.
pub const HARD_MAX_TOTAL_MB: u64 = 16 * 1024;
pub const MAX_POINTS_PER_FRAME: usize = 4096;
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
pub const MAX_DIM: usize = 8192;
pub const MAX_TOP_K: usize = 200;

#[derive(Clone, Debug, Default)]
pub struct VectorsConfig {
    /// Lend vector storage at all. Off by default.
    pub enabled: bool,
    /// Ceiling on the bytes all collections may occupy here, MB.
    pub max_mb: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Point {
    v: Vec<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    p: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Shard {
    dim: usize,
    metric: String,
    points: HashMap<String, Point>,
}

impl Shard {
    fn bytes(&self) -> u64 {
        self.points
            .values()
            .map(|p| (p.v.len() * 4 + p.p.as_ref().map_or(0, |s| s.len()) + 64) as u64)
            .sum()
    }
}

/// The replica store: every collection assigned to this node, in memory,
/// persisted as one JSON file per collection under `<node dir>/vectors`.
pub struct Store {
    dir: PathBuf,
    cfg: VectorsConfig,
    shards: Mutex<HashMap<String, Shard>>,
}

pub type SharedStore = Arc<Store>;

impl Store {
    pub fn new(node_dir: &Path, cfg: VectorsConfig) -> Self {
        Store {
            dir: node_dir.join("vectors"),
            cfg,
            shards: Mutex::new(HashMap::new()),
        }
    }

    /// Read every collection file back. Called once at start so a restart
    /// does not lose replicas; a file that does not parse is skipped and
    /// logged rather than taking the node down.
    pub async fn load(&self) -> usize {
        let mut loaded = 0;
        let Ok(mut rd) = tokio::fs::read_dir(&self.dir).await else {
            return 0;
        };
        let mut shards = self.shards.lock().await;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("json") || !collection_id_ok(stem)
            {
                continue;
            }
            match tokio::fs::read(&path).await {
                Ok(bytes) => match serde_json::from_slice::<Shard>(&bytes) {
                    Ok(shard) => {
                        shards.insert(stem.to_string(), shard);
                        loaded += 1;
                    }
                    Err(e) => println!("[vectors] {}: unreadable, skipped ({e})", path.display()),
                },
                Err(e) => println!("[vectors] {}: unreadable, skipped ({e})", path.display()),
            }
        }
        loaded
    }

    /// Bytes all collections occupy, MB, for the capability and the pong.
    pub async fn used_mb(&self) -> u64 {
        let shards = self.shards.lock().await;
        shards.values().map(Shard::bytes).sum::<u64>() / 1_048_576
    }

    /// Collection ids held here, announced on hello so the gateway can tell
    /// this node to drop any it no longer knows.
    pub async fn collection_ids(&self) -> Vec<String> {
        self.shards.lock().await.keys().cloned().collect()
    }

    /// The hello frame's `vectors` block. Null when the lane is off.
    pub async fn capability(&self) -> Value {
        if !self.cfg.enabled {
            return Value::Null;
        }
        json!({
            "enabled": true,
            "max_mb": self.cfg.max_mb.clamp(1, HARD_MAX_TOTAL_MB),
            "used_mb": self.used_mb().await,
        })
    }

    async fn persist(&self, id: &str, shard: Option<&Shard>) -> Result<(), String> {
        let path = self.dir.join(format!("{id}.json"));
        match shard {
            None => {
                let _ = tokio::fs::remove_file(&path).await;
                Ok(())
            }
            Some(s) => {
                tokio::fs::create_dir_all(&self.dir)
                    .await
                    .map_err(|e| e.to_string())?;
                let tmp = self.dir.join(format!("{id}.json.tmp"));
                let bytes = serde_json::to_vec(s).map_err(|e| e.to_string())?;
                tokio::fs::write(&tmp, &bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                tokio::fs::rename(&tmp, &path)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// `vector_upsert`: create the collection on first sight (the gateway
    /// placed it here), then insert or replace each point.
    pub async fn upsert(&self, payload: &Value) -> Result<Value, String> {
        if !self.cfg.enabled {
            return Err("this node does not lend vector storage".into());
        }
        let id = collection_id(payload)?;
        let dim = payload.get("dim").and_then(Value::as_u64).unwrap_or(0) as usize;
        let metric = payload
            .get("metric")
            .and_then(Value::as_str)
            .unwrap_or("cosine")
            .to_string();
        if dim == 0 || dim > MAX_DIM {
            return Err(format!("dim must be 1..{MAX_DIM}"));
        }
        if !matches!(metric.as_str(), "cosine" | "dot" | "euclid") {
            return Err(format!("unknown metric {metric:?}"));
        }
        let points = payload
            .get("points")
            .and_then(Value::as_array)
            .ok_or("points must be a list")?;
        if points.is_empty() || points.len() > MAX_POINTS_PER_FRAME {
            return Err(format!(
                "points must hold 1..{MAX_POINTS_PER_FRAME} entries"
            ));
        }
        let mut parsed: Vec<(String, Point)> = Vec::with_capacity(points.len());
        for (i, p) in points.iter().enumerate() {
            let pid = p.get("id").and_then(Value::as_str).unwrap_or("").trim();
            if pid.is_empty() || pid.len() > 128 {
                return Err(format!("points[{i}]: bad id"));
            }
            let v = parse_vector(p.get("vector"), dim).map_err(|e| format!("points[{i}]: {e}"))?;
            let payload_b64 = p
                .get("payload_b64")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(b) = &payload_b64 {
                if b.len() * 3 / 4 > MAX_PAYLOAD_BYTES {
                    return Err(format!(
                        "points[{i}]: payload exceeds {MAX_PAYLOAD_BYTES} bytes"
                    ));
                }
            }
            parsed.push((pid.to_string(), Point { v, p: payload_b64 }));
        }

        let mut shards = self.shards.lock().await;
        if let Some(existing) = shards.get(&id) {
            if existing.dim != dim {
                return Err(format!(
                    "collection has dim {}, points have {dim}",
                    existing.dim
                ));
            }
        }
        // The ceiling is checked BEFORE the collection is created: a refused
        // first write must leave nothing behind, or an empty collection would
        // be announced on the next hello for an index that was never stored.
        let total_before: u64 = shards.values().map(Shard::bytes).sum();
        let incoming: u64 = parsed
            .iter()
            .map(|(_, p)| (p.v.len() * 4 + p.p.as_ref().map_or(0, |s| s.len()) + 64) as u64)
            .sum();
        let cap = self.cfg.max_mb.clamp(1, HARD_MAX_TOTAL_MB) * 1_048_576;
        if total_before + incoming > cap {
            return Err(format!(
                "this provider's vector storage ceiling ({} MB) would be exceeded",
                cap / 1_048_576
            ));
        }
        let shard = shards.entry(id.clone()).or_insert_with(|| Shard {
            dim,
            metric: metric.clone(),
            points: HashMap::new(),
        });
        for (pid, point) in parsed {
            shard.points.insert(pid, point);
        }
        let count = shard.points.len();
        let snapshot = shard.clone();
        drop(shards);
        self.persist(&id, Some(&snapshot)).await?;
        Ok(json!({"collection": id, "points": count}))
    }

    /// `vector_query`: the `top_k` nearest points by the collection's metric.
    pub async fn query(&self, payload: &Value) -> Result<Value, String> {
        let id = collection_id(payload)?;
        let top_k = payload
            .get("top_k")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, MAX_TOP_K as u64) as usize;
        let shards = self.shards.lock().await;
        let shard = shards.get(&id).ok_or("collection not held on this node")?;
        let q = parse_vector(payload.get("vector"), shard.dim)?;
        let mut scored: Vec<(f32, &String, &Point)> = shard
            .points
            .iter()
            .map(|(pid, p)| (score(&shard.metric, &q, &p.v), pid, p))
            .collect();
        // Higher is better for cosine/dot; euclid is stored as a negative
        // distance so one ordering serves all three.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let matches: Vec<Value> = scored
            .into_iter()
            .take(top_k)
            .map(|(s, pid, p)| {
                let mut m = json!({"id": pid, "score": s});
                if let Some(b) = &p.p {
                    m["payload_b64"] = json!(b);
                }
                m
            })
            .collect();
        Ok(json!({"collection": id, "matches": matches}))
    }

    /// `vector_delete`: remove points by id. Unknown ids are not an error.
    pub async fn delete(&self, payload: &Value) -> Result<Value, String> {
        let id = collection_id(payload)?;
        let ids: Vec<String> = payload
            .get("ids")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut shards = self.shards.lock().await;
        let Some(shard) = shards.get_mut(&id) else {
            return Ok(json!({"collection": id, "deleted": 0}));
        };
        let mut deleted = 0;
        for pid in &ids {
            if shard.points.remove(pid).is_some() {
                deleted += 1;
            }
        }
        let snapshot = shard.clone();
        drop(shards);
        self.persist(&id, Some(&snapshot)).await?;
        Ok(json!({"collection": id, "deleted": deleted}))
    }

    /// `vector_drop`: forget the collection and its file.
    pub async fn drop_collection(&self, payload: &Value) -> Result<Value, String> {
        let id = collection_id(payload)?;
        let existed = self.shards.lock().await.remove(&id).is_some();
        self.persist(&id, None).await?;
        Ok(json!({"collection": id, "dropped": existed}))
    }
}

fn collection_id(payload: &Value) -> Result<String, String> {
    let id = payload
        .get("collection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !collection_id_ok(&id) {
        return Err("collection must be a 32-character hex id".into());
    }
    Ok(id)
}

/// A collection id names a FILE under the node dir, so it is held to the
/// exact shape the gateway mints (uuid4 hex) and nothing else.
pub fn collection_id_ok(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_vector(v: Option<&Value>, dim: usize) -> Result<Vec<f32>, String> {
    let arr = v.and_then(Value::as_array).ok_or("vector must be a list")?;
    if arr.len() != dim {
        return Err(format!(
            "vector must have {dim} dimensions, has {}",
            arr.len()
        ));
    }
    let mut out = Vec::with_capacity(dim);
    for x in arr {
        let f = x.as_f64().ok_or("vector must contain numbers")? as f32;
        if !f.is_finite() {
            return Err("vector contains NaN or infinity".into());
        }
        out.push(f);
    }
    Ok(out)
}

/// Similarity, higher is closer. Euclid returns the negated distance so the
/// same descending sort applies.
pub fn score(metric: &str, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        "dot" => dot(a, b),
        "euclid" => -a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            .sqrt(),
        _ => {
            let na = dot(a, a).sqrt();
            let nb = dot(b, b).sqrt();
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                dot(a, b) / (na * nb)
            }
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(max_mb: u64) -> (tempfile_dir::Dir, Store) {
        let dir = tempfile_dir::Dir::new();
        let s = Store::new(
            dir.path(),
            VectorsConfig {
                enabled: true,
                max_mb,
            },
        );
        (dir, s)
    }

    const CID: &str = "0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn upsert_query_delete_drop_round_trip() {
        let (_d, s) = store(64);
        let up = json!({"collection": CID, "dim": 3, "metric": "cosine", "points": [
            {"id": "a", "vector": [1.0, 0.0, 0.0], "payload_b64": "QQ=="},
            {"id": "b", "vector": [0.0, 1.0, 0.0]},
            {"id": "c", "vector": [0.9, 0.1, 0.0]},
        ]});
        assert_eq!(s.upsert(&up).await.unwrap()["points"], 3);
        let q = s
            .query(&json!({"collection": CID, "vector": [1.0, 0.0, 0.0], "top_k": 2}))
            .await
            .unwrap();
        let ids: Vec<&str> = q["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "c"]);
        assert_eq!(q["matches"][0]["payload_b64"], "QQ==");
        assert!(q["matches"][1].get("payload_b64").is_none());
        assert_eq!(
            s.delete(&json!({"collection": CID, "ids": ["a", "zz"]}))
                .await
                .unwrap()["deleted"],
            1
        );
        assert_eq!(s.collection_ids().await, vec![CID.to_string()]);
        assert_eq!(
            s.drop_collection(&json!({"collection": CID}))
                .await
                .unwrap()["dropped"],
            true
        );
        assert!(s.collection_ids().await.is_empty());
        assert!(s
            .query(&json!({"collection": CID, "vector": [1.0, 0.0, 0.0]}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn collections_survive_a_restart() {
        let dir = tempfile_dir::Dir::new();
        {
            let s = Store::new(
                dir.path(),
                VectorsConfig {
                    enabled: true,
                    max_mb: 64,
                },
            );
            s.upsert(&json!({"collection": CID, "dim": 2, "points": [{"id": "a", "vector": [1.0, 2.0]}]}))
                .await
                .unwrap();
        }
        let s = Store::new(
            dir.path(),
            VectorsConfig {
                enabled: true,
                max_mb: 64,
            },
        );
        assert_eq!(s.load().await, 1);
        let q = s
            .query(&json!({"collection": CID, "vector": [1.0, 2.0]}))
            .await
            .unwrap();
        assert_eq!(q["matches"][0]["id"], "a");
    }

    #[tokio::test]
    async fn the_owners_ceiling_is_enforced_before_anything_is_written() {
        let (_d, s) = store(1); // 1 MB
        let big: Vec<f32> = vec![0.5; 4096];
        let points: Vec<Value> = (0..80)
            .map(|i| json!({"id": format!("p{i}"), "vector": big}))
            .collect();
        // 80 * 16 KB = 1.25 MB > 1 MB ceiling.
        let err = s
            .upsert(&json!({"collection": CID, "dim": 4096, "points": points}))
            .await
            .unwrap_err();
        assert!(err.contains("ceiling"), "{err}");
        assert!(s.collection_ids().await.is_empty(), "nothing was kept");
    }

    #[tokio::test]
    async fn malformed_frames_are_refused() {
        let (_d, s) = store(64);
        assert!(s.upsert(&json!({"collection": "../etc/passwd", "dim": 2, "points": [{"id": "a", "vector": [1.0, 2.0]}]})).await.is_err());
        assert!(s
            .upsert(&json!({"collection": CID, "dim": 2, "points": [{"id": "a", "vector": [1.0]}]}))
            .await
            .is_err());
        assert!(s
            .upsert(
                &json!({"collection": CID, "dim": 2, "points": [{"id": "", "vector": [1.0, 2.0]}]})
            )
            .await
            .is_err());
        assert!(s.upsert(&json!({"collection": CID, "dim": 2, "metric": "manhattan", "points": [{"id": "a", "vector": [1.0, 2.0]}]})).await.is_err());
        assert!(s
            .upsert(&json!({"collection": CID, "dim": 2, "points": []}))
            .await
            .is_err());
        let off = Store::new(
            _d.path(),
            VectorsConfig {
                enabled: false,
                max_mb: 64,
            },
        );
        assert!(off
            .upsert(
                &json!({"collection": CID, "dim": 2, "points": [{"id": "a", "vector": [1.0, 2.0]}]})
            )
            .await
            .is_err());
        assert!(off.capability().await.is_null());
    }

    #[test]
    fn metrics_rank_as_expected() {
        assert!((score("cosine", &[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert_eq!(score("cosine", &[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(score("dot", &[1.0, 2.0], &[3.0, 4.0]), 11.0);
        assert!(
            score("euclid", &[0.0, 0.0], &[0.0, 1.0]) > score("euclid", &[0.0, 0.0], &[0.0, 2.0])
        );
        assert!(collection_id_ok(CID));
        assert!(!collection_id_ok("short"));
        assert!(!collection_id_ok(&"g".repeat(32)));
    }

    /// A throwaway directory without pulling in a crate for it.
    mod tempfile_dir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                // Unique by CONSTRUCTION, not by clock. The name used to be
                // pid + nanos, and two tests starting inside the same clock
                // tick shared a directory — so one test's cleanup deleted the
                // other's store mid-write and the failure ("No such file or
                // directory") pointed at the code under test rather than at
                // the harness. It reproduced roughly one run in three once
                // enough tests ran alongside these.
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let p = std::env::temp_dir().join(format!(
                    "kmplify-node-vectors-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
