//! Peer Compute provider terms for document content (DSGVO, owner go
//! 2026-11-16). Same contract as kmplify-desktop's provider_terms.rs; the
//! headless node takes the acceptance from `PROVIDER_DOCUMENT_TERMS`.
//!
//! A consumer's page-vision request (a rendered PDF page, a scan) carries
//! personal data. The provider that runs it processes that data for the
//! consumer, so it needs an Art. 28 basis: the KMPLIFY Peer Compute provider
//! terms (`Repositories/docs/peer-compute/PEER-COMPUTE-PROVIDER-TERMS-2026-11.md`,
//! a draft for the lawyer). This module records that the operator of THIS
//! machine accepted them, and which version.
//!
//! - Stored locally next to the fabric credentials as
//!   `provider-document-terms.json` (`{"version", "accepted_at"}`), never in
//!   a settings file and never anything but those two facts.
//! - Announced to the gateway in every hello (`document_terms`), which then
//!   schedules jobs marked `content: "document"` only onto nodes whose
//!   version equals the gateway's current one.
//! - Checked again by the worker on every such job: a missing or older
//!   acceptance refuses the job before anything reaches the local engine.
//!   The same refusal applies while the local engine would log prompts
//!   (`OLLAMA_DEBUG` set in this process's environment).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The terms version this build asks providers to accept. Bumping it makes
/// every earlier acceptance "older", which means no document jobs until the
/// operator accepts again. Must equal the gateway's `DOCUMENT_TERMS_VERSION`.
pub const CURRENT_VERSION: &str = "2026-11";

const FILE: &str = "provider-document-terms.json";

/// What the worker answers instead of running a document job it may not run.
/// A machine code first, so the consumer can tell it apart from an engine
/// failure; no request content in it.
pub const REFUSAL_CODE: &str = "peer_document_terms_missing";
pub const DEBUG_REFUSAL_CODE: &str = "peer_document_engine_logging";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acceptance {
    pub version: String,
    /// Unix seconds when the operator accepted.
    pub accepted_at: u64,
}

impl Acceptance {
    pub fn is_current(&self) -> bool {
        self.version == CURRENT_VERSION
    }
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The stored acceptance, or None when there is none or it cannot be read
/// (a damaged file means "not accepted", never "accepted").
pub fn load(dir: &Path) -> Option<Acceptance> {
    let bytes = std::fs::read(path(dir)).ok()?;
    let a: Acceptance = serde_json::from_slice(&bytes).ok()?;
    (!a.version.trim().is_empty() && a.accepted_at > 0).then_some(a)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record that the operator accepted `version`. Only the version this build
/// shows can be accepted: accepting a text the operator never saw is refused.
/// Accepting the version already on file keeps its original time.
pub fn accept(dir: &Path, version: &str) -> Result<Acceptance, String> {
    if version != CURRENT_VERSION {
        return Err(format!(
            "terms version '{version}' is not the current one ({CURRENT_VERSION}); reload the page and read the current terms"
        ));
    }
    if let Some(a) = load(dir) {
        if a.is_current() {
            return Ok(a);
        }
    }
    let a = Acceptance {
        version: CURRENT_VERSION.to_string(),
        accepted_at: now_secs(),
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = crate::node_dir::unique_tmp(&path(dir));
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&a).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path(dir)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(a)
}

/// Withdraw the acceptance: from the next connection on, no document jobs.
pub fn withdraw(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(path(dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Ollama writes every prompt into its server log when `OLLAMA_DEBUG` is on
/// (its "chat request" debug line). An engine started from this user's
/// environment inherits the variable, so a provider whose environment has it
/// would keep document text in a log file. Truthy as Ollama reads it: any
/// value except empty, "0" and "false".
pub fn engine_logs_prompts() -> bool {
    engine_logs_prompts_from(std::env::var("OLLAMA_DEBUG").ok().as_deref())
}

fn engine_logs_prompts_from(v: Option<&str>) -> bool {
    match v.map(|s| s.trim().to_ascii_lowercase()) {
        None => false,
        Some(s) => !(s.is_empty() || s == "0" || s == "false"),
    }
}

/// The `document_terms` block of the hello frame, or Null without a current
/// acceptance (and while the engine logs prompts: such a node must not be
/// offered document jobs at all).
pub fn hello_block(dir: Option<&Path>) -> serde_json::Value {
    match dir.and_then(load) {
        Some(a) if a.is_current() && !engine_logs_prompts() => {
            serde_json::json!({"version": a.version, "accepted_at": a.accepted_at})
        }
        _ => serde_json::Value::Null,
    }
}

/// Whether a job frame carries document content (gateway protocol: the job
/// frame's `content` is "document").
pub fn is_document_job(frame: &serde_json::Value) -> bool {
    frame.get("content").and_then(serde_json::Value::as_str) == Some("document")
}

/// None when this machine may run the document job, else the refusal code.
pub fn refusal_for(dir: Option<&Path>) -> Option<&'static str> {
    if !dir.and_then(load).is_some_and(|a| a.is_current()) {
        return Some(REFUSAL_CODE);
    }
    if engine_logs_prompts() {
        return Some(DEBUG_REFUSAL_CODE);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kmplify-terms-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn nothing_on_file_means_no_document_jobs() {
        let d = tmp();
        assert_eq!(load(&d), None);
        assert_eq!(refusal_for(Some(&d)), Some(REFUSAL_CODE));
        assert_eq!(refusal_for(None), Some(REFUSAL_CODE));
        assert!(hello_block(Some(&d)).is_null());
    }

    #[test]
    fn accepting_the_current_version_is_recorded_and_announced() {
        let d = tmp();
        let a = accept(&d, CURRENT_VERSION).unwrap();
        assert!(a.is_current() && a.accepted_at > 0);
        assert_eq!(load(&d), Some(a.clone()));
        // Accepting again keeps the first time.
        assert_eq!(
            accept(&d, CURRENT_VERSION).unwrap().accepted_at,
            a.accepted_at
        );
        if !engine_logs_prompts() {
            assert_eq!(refusal_for(Some(&d)), None);
            assert_eq!(hello_block(Some(&d))["version"], CURRENT_VERSION);
        }
        // Only the version and the time are on file.
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path(&d)).unwrap()).unwrap();
        assert_eq!(raw.as_object().unwrap().len(), 2);
    }

    #[test]
    fn an_older_or_unknown_version_counts_as_missing() {
        let d = tmp();
        assert!(
            accept(&d, "2026-01").is_err(),
            "a version this build does not show cannot be accepted"
        );
        std::fs::write(
            path(&d),
            r#"{"version":"2026-01","accepted_at":1700000000}"#,
        )
        .unwrap();
        assert_eq!(refusal_for(Some(&d)), Some(REFUSAL_CODE));
        assert!(hello_block(Some(&d)).is_null());
        std::fs::write(path(&d), "not json").unwrap();
        assert_eq!(load(&d), None, "a damaged file is never an acceptance");
    }

    #[test]
    fn withdrawing_ends_document_jobs() {
        let d = tmp();
        accept(&d, CURRENT_VERSION).unwrap();
        withdraw(&d).unwrap();
        withdraw(&d).unwrap();
        assert_eq!(refusal_for(Some(&d)), Some(REFUSAL_CODE));
    }

    #[test]
    fn ollama_debug_reads_as_ollama_reads_it() {
        assert!(!engine_logs_prompts_from(None));
        assert!(!engine_logs_prompts_from(Some("")));
        assert!(!engine_logs_prompts_from(Some("0")));
        assert!(!engine_logs_prompts_from(Some("false")));
        assert!(engine_logs_prompts_from(Some("1")));
        assert!(engine_logs_prompts_from(Some("true")));
        assert!(engine_logs_prompts_from(Some("2")));
    }

    #[test]
    fn only_a_literal_document_marker_is_a_document_job() {
        assert!(is_document_job(
            &serde_json::json!({"type": "job", "content": "document"})
        ));
        assert!(!is_document_job(&serde_json::json!({"type": "job"})));
        assert!(!is_document_job(
            &serde_json::json!({"type": "job", "content": "chat"})
        ));
    }
}
