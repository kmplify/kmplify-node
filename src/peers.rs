//! Who is allowed to use this machine.
//!
//! The other half of the desktop app's provider panel: the consumers using
//! this node, the ones waiting for a decision when manual admission is on,
//! and the invitations this node has minted. All of it lives on the gateway
//! (it is the party that sees consumers), so this module is a small
//! provider-side client for the `/fabric/*` endpoints, authenticated with the
//! node's own credential.
//!
//! It exists because of one specific trap: the dashboard can now turn
//! **manual approval** on, and a node in manual mode with no way to approve
//! anybody is a node that has quietly stopped serving. Every switch the
//! terminal offers has to come with the screen that makes it usable.
//!
//! Nothing here is required for a node to serve. A gateway that has not got
//! these endpoints, or a network that cannot reach it, degrades to an empty
//! list and a message — never to a worker that stops working.

use std::path::Path;

use serde::Deserialize;

/// One consumer waiting for a decision (manual admission only).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Pending {
    /// Node id for identified consumers, `anon-<hash8>` otherwise.
    pub consumer: String,
    pub first_seen_seconds: i64,
    pub last_seen_seconds: i64,
    /// What they asked for last, which is usually the only clue to who they
    /// are.
    pub model: String,
}

/// One consumer this node has served recently.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Consumer {
    pub consumer: String,
    /// How the last work arrived: `invitation <id8>`, `grid selection`, `pool`.
    pub via: String,
    pub active: bool,
    pub connected_for_seconds: i64,
    pub last_seen_seconds: i64,
    /// The standing decision: approved | denied | blocked | none.
    pub rule: Option<String>,
}

/// A minted invitation: the connection contract for one consumer.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Invitation {
    pub invitation_id: String,
    pub invite_url: String,
    pub label: String,
    /// Provider-side valve: pinned requests are refused until resumed.
    pub paused: bool,
    pub consumer_active: bool,
    pub connected_for_seconds: i64,
    pub revoked: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ConsumersResponse {
    consumers: Vec<Consumer>,
    approval_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ApprovalsResponse {
    pending: Vec<Pending>,
}

/// Everything the peers screen shows, fetched together.
#[derive(Clone, Debug, Default)]
pub struct Peers {
    pub pending: Vec<Pending>,
    pub consumers: Vec<Consumer>,
    pub invitations: Vec<Invitation>,
    /// The admission mode the gateway believes this node advertised, or None
    /// while the node is offline. Worth showing next to the local setting:
    /// they disagree exactly while a change has not been re-advertised yet.
    pub approval_mode: Option<String>,
}

/// The node's own credential, read without registering a new one.
///
/// Deliberately not [`crate::fabric_worker::ensure_identity`]: a dashboard
/// that cannot find a credential must say so, not mint an identity and join a
/// fabric as a side effect of opening a screen.
pub fn credential(creds_path: &Path) -> Option<crate::fabric_worker::Credentials> {
    let bytes = std::fs::read(creds_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn client(timeout: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default()
}

/// Fetch the three lists, concurrently.
pub async fn fetch(
    gateway: &str,
    token: &str,
    timeout: std::time::Duration,
) -> Result<Peers, String> {
    let c = client(timeout);
    let (approvals, consumers, invitations) = tokio::join!(
        get::<ApprovalsResponse>(&c, gateway, token, "/fabric/approvals"),
        get::<ConsumersResponse>(&c, gateway, token, "/fabric/consumers"),
        get::<Vec<Invitation>>(&c, gateway, token, "/fabric/invitations"),
    );
    // One failing list must not blank the other two: the pending queue is the
    // urgent one, and it should still render when the invitation list is what
    // the gateway is unhappy about.
    let mut peers = Peers::default();
    let mut trouble = Vec::new();
    match approvals {
        Ok(a) => peers.pending = a.pending,
        Err(e) => trouble.push(format!("approvals: {e}")),
    }
    match consumers {
        Ok(c) => {
            peers.consumers = c.consumers;
            peers.approval_mode = c.approval_mode;
        }
        Err(e) => trouble.push(format!("consumers: {e}")),
    }
    match invitations {
        Ok(i) => peers.invitations = i,
        Err(e) => trouble.push(format!("invitations: {e}")),
    }
    if trouble.len() == 3 {
        return Err(trouble.join("; "));
    }
    Ok(peers)
}

async fn get<T: serde::de::DeserializeOwned>(
    c: &reqwest::Client,
    gateway: &str,
    token: &str,
    path: &str,
) -> Result<T, String> {
    c.get(format!("{gateway}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// Decide about one consumer.
///
/// `decision` is `approved`, `denied`, `blocked`, or `None` to clear the
/// standing rule and go back to whatever the admission mode says.
pub async fn decide(
    gateway: &str,
    token: &str,
    consumer: &str,
    decision: Option<&str>,
    timeout: std::time::Duration,
) -> Result<(), String> {
    client(timeout)
        .post(format!("{gateway}/fabric/approvals"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "consumer": consumer, "decision": decision }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Mint an invitation addressed to one consumer.
pub async fn invite(
    gateway: &str,
    token: &str,
    label: &str,
    timeout: std::time::Duration,
) -> Result<Invitation, String> {
    client(timeout)
        .post(format!("{gateway}/fabric/invitations"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "label": label }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// Revoke an invitation permanently.
pub async fn revoke(
    gateway: &str,
    token: &str,
    invitation_id: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    client(timeout)
        .delete(format!("{gateway}/fabric/invitations/{invitation_id}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Pause or resume an invitation without revoking it — the reversible half of
/// "not right now".
pub async fn set_paused(
    gateway: &str,
    token: &str,
    inv: &Invitation,
    paused: bool,
    timeout: std::time::Duration,
) -> Result<(), String> {
    client(timeout)
        .put(format!(
            "{gateway}/fabric/invitations/{}",
            inv.invitation_id
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "label": inv.label, "paused": paused }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateways_shapes_parse_into_ours() {
        // Field-for-field against gateway/app/schemas.py; a rename there must
        // fail here rather than render an empty screen.
        let a: ApprovalsResponse = serde_json::from_str(
            r#"{"pending":[{"consumer":"anon-1a2b3c4d","first_seen_seconds":90,
                 "last_seen_seconds":3,"model":"llama3"}],"decided":[]}"#,
        )
        .unwrap();
        assert_eq!(a.pending[0].consumer, "anon-1a2b3c4d");
        assert_eq!(a.pending[0].model, "llama3");

        let c: ConsumersResponse = serde_json::from_str(
            r#"{"consumers":[{"consumer":"node-9","via":"grid selection","active":true,
                 "connected_for_seconds":42,"last_seen_seconds":1,"rule":null}],
                 "approval_mode":"manual"}"#,
        )
        .unwrap();
        assert!(c.consumers[0].active);
        assert_eq!(c.approval_mode.as_deref(), Some("manual"));
        assert_eq!(c.consumers[0].rule, None);

        let i: Vec<Invitation> = serde_json::from_str(
            r#"[{"invitation_id":"7f9b2c9e-4a1d-4e5f-9c3a-2b8d1e6f0a47",
                 "invite_url":"https://fabric.kmplify.io/i/7f9b","label":"Anna's phone",
                 "paused":false,"consumer_active":true,"connected_for_seconds":10,
                 "created":1.0,"revoked":false}]"#,
        )
        .unwrap();
        assert_eq!(i[0].label, "Anna's phone");
        assert!(i[0].consumer_active);
    }

    #[test]
    fn a_gateway_that_answers_with_less_still_renders() {
        // Older gateway, or one that omits the newer fields: every field
        // defaults, so the screen loses a column rather than the whole list.
        let c: ConsumersResponse =
            serde_json::from_str(r#"{"consumers":[{"consumer":"node-9"}]}"#).unwrap();
        assert_eq!(c.consumers[0].consumer, "node-9");
        assert!(!c.consumers[0].active);
        assert_eq!(c.approval_mode, None);
    }

    #[test]
    fn a_missing_credential_is_not_a_new_identity() {
        let missing = std::env::temp_dir().join("kmplify-node-no-such-cred.json");
        let _ = std::fs::remove_file(&missing);
        assert!(credential(&missing).is_none());
    }
}
