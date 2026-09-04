//! Who this node trusts, and how that trust is established.
//!
//! Every node holds a self-signed leaf certificate. A cluster is the set of
//! nodes that have **pinned** each other's certificates: pairing exchanges
//! the two certificates under a six-digit PIN, and from then on every peer
//! surface — node-info, the proxies — is mutual TLS in which each side
//! checks the other's certificate fingerprint against its pins and nothing
//! else. No certificate authority, no hostname matching, no clock: a
//! fingerprint either is on the list or is not.
//!
//! The PIN authenticates a SPAKE2 key exchange rather than being sent or
//! hashed on the wire. That is the difference between "a convenience code"
//! and "a secret an attacker on the same Wi-Fi can recover in a second": a
//! passive listener learns nothing it can brute-force offline, and an
//! active man in the middle gets one online guess per attempt, of which an
//! invite allows three before it closes. PAIR uses EAP-NOOB for the same
//! step; SPAKE2 is the smaller standard construction with the same
//! property, and it is what fits in one file anyone can read.
//!
//! Files, in `<node dir>/router/`, all owner-only:
//! `node.crt.der`, `node.key.der` (PKCS#8) and `cluster.json`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};

use super::{lock, node_info_port, Router, Shared};

pub const DIR: &str = "router";
const CERT_FILE: &str = "node.crt.der";
const KEY_FILE: &str = "node.key.der";
const CLUSTER_FILE: &str = "cluster.json";

/// An invite is good for this long, and for this many wrong PINs.
pub const INVITE_TTL: Duration = Duration::from_secs(5 * 60);
pub const INVITE_ATTEMPTS: u32 = 3;

/// The SPAKE2 identity string both sides use in symmetric mode. Fixed, so
/// two copies of this program agree without exchanging it.
const SPAKE_ID: &[u8] = b"kmplify-node pairing v1";

// ------------------------------------------------------------------ identity

/// This node's certificate and key, as rustls wants them.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

pub fn fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Short form for screens: enough to compare by eye, not to type.
pub fn short_fp(fp: &str) -> String {
    fp.chars().take(16).collect()
}

fn dir(node_dir: &Path) -> PathBuf {
    node_dir.join(DIR)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl Identity {
    /// Load the certificate this install already has, or mint one. Minting
    /// happens once per install; the fingerprint peers pinned would change
    /// otherwise and every pairing would be undone.
    pub fn load_or_create(node_dir: &Path, node_id: &str) -> Result<Self, String> {
        let d = dir(node_dir);
        let cert_path = d.join(CERT_FILE);
        let key_path = d.join(KEY_FILE);
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            if !cert.is_empty() && !key.is_empty() {
                return Ok(Self::from_der(cert, key));
            }
        }
        let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("key generation: {e}"))?;
        let host = super::hostname();
        let mut params = rcgen::CertificateParams::new(vec![
            format!("{}.kmplify-node.local", &node_id[..12.min(node_id.len())]),
        ])
        .map_err(|e| format!("certificate parameters: {e}"))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("kmplify-node {host}"));
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| format!("certificate: {e}"))?;
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();
        write_private(&cert_path, &cert_der)
            .map_err(|e| format!("{}: {e}", cert_path.display()))?;
        write_private(&key_path, &key_der).map_err(|e| format!("{}: {e}", key_path.display()))?;
        Ok(Self::from_der(cert_der, key_der))
    }

    fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        let fingerprint = fingerprint(&cert);
        Self {
            cert: CertificateDer::from(cert),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            fingerprint,
        }
    }
}

// ------------------------------------------------------------------ the cluster file

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    #[serde(default)]
    pub added_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClusterFile {
    /// Empty = this node belongs to no cluster.
    #[serde(default)]
    pub cluster_id: String,
    #[serde(default)]
    pub members: BTreeMap<String, Member>,
    /// Nodes removed on purpose, so a member's report does not quietly add
    /// them back. Cleared by pairing with them again.
    #[serde(default)]
    pub removed: BTreeMap<String, u64>,
}

impl ClusterFile {
    pub fn load(node_dir: &Path) -> Self {
        std::fs::read(dir(node_dir).join(CLUSTER_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, node_dir: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        write_private(&dir(node_dir).join(CLUSTER_FILE), &bytes)
    }

    pub fn is_clustered(&self) -> bool {
        !self.cluster_id.is_empty()
    }

    pub fn member_by_fingerprint(&self, fp: &str) -> Option<&Member> {
        self.members.values().find(|m| m.fingerprint == fp)
    }

    pub fn fingerprints(&self) -> HashSet<String> {
        self.members.values().map(|m| m.fingerprint.clone()).collect()
    }
}

// ------------------------------------------------------------------ pins and TLS

/// The live set of trusted fingerprints, shared with every verifier so a
/// pairing takes effect on the next handshake without rebuilding a config.
#[derive(Clone, Default)]
pub struct Pins(Arc<RwLock<HashSet<String>>>);

impl std::fmt::Debug for Pins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pins({})", self.count())
    }
}

impl Pins {
    pub fn set(&self, fps: HashSet<String>) {
        if let Ok(mut w) = self.0.write() {
            *w = fps;
        }
    }

    pub fn contains(&self, fp: &str) -> bool {
        self.0.read().map(|r| r.contains(fp)).unwrap_or(false)
    }

    pub fn count(&self) -> usize {
        self.0.read().map(|r| r.len()).unwrap_or(0)
    }
}

/// Accepts exactly the certificates whose fingerprint is pinned. Used on
/// both ends: as the client's view of a server and the server's view of a
/// client, because the relationship is symmetric.
#[derive(Debug)]
struct PinnedVerifier {
    pins: Pins,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.pins.contains(&fingerprint(end_entity.as_ref())) {
            Ok(())
        } else {
            Err(rustls::Error::General("certificate is not pinned by this node".into()))
        }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The server side of every peer surface: presents this node's certificate,
/// demands the peer's, accepts only pinned ones.
pub fn server_config(identity: &Identity, pins: &Pins) -> Result<Arc<ServerConfig>, String> {
    let provider = provider();
    let verifier = Arc::new(PinnedVerifier {
        pins: pins.clone(),
        provider: provider.clone(),
    });
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// The client side: the same verifier for the peer's certificate, this
/// node's own as the client certificate.
pub fn client_config(identity: &Identity, pins: &Pins) -> Result<ClientConfig, String> {
    let provider = provider();
    let verifier = Arc::new(PinnedVerifier {
        pins: pins.clone(),
        provider: provider.clone(),
    });
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key.clone_key())
        .map_err(|e| e.to_string())
}

/// An HTTP client that speaks the cluster's mutual TLS. Hostnames are not
/// checked (the fingerprint is the identity), so an `https://<ip>:port`
/// URL is exactly right.
pub fn tls_client(identity: &Identity, pins: &Pins) -> Result<reqwest::Client, String> {
    let cfg = client_config(identity, pins)?;
    reqwest::Client::builder()
        .use_preconfigured_tls(cfg)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ pairing

/// An open invitation: the PIN on this screen, and the exchanges in flight.
#[derive(Clone, Debug)]
pub struct Invite {
    pub pin: String,
    pub created: Instant,
    pub wrong_attempts: u32,
    /// Sessions that completed the key exchange and await confirmation.
    sessions: BTreeMap<String, Session>,
}

#[derive(Clone, Debug)]
struct Session {
    key: Vec<u8>,
    joiner: Member,
    transcript: Vec<u8>,
    /// Where the joiner came from and where its node-info answers, so
    /// the inviter can poll it the moment pairing completes.
    joiner_addr: Option<std::net::IpAddr>,
    joiner_info_port: u16,
}

impl Invite {
    pub fn new() -> Self {
        use rand::Rng;
        let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
        Self {
            pin: format!("{n:06}"),
            created: Instant::now(),
            wrong_attempts: 0,
            sessions: BTreeMap::new(),
        }
    }

    pub fn expired(&self) -> bool {
        self.created.elapsed() > INVITE_TTL || self.wrong_attempts >= INVITE_ATTEMPTS
    }

    pub fn remaining(&self) -> Duration {
        INVITE_TTL.saturating_sub(self.created.elapsed())
    }
}

impl Default for Invite {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "lowercase")]
pub enum PairRequest {
    Start {
        joiner_id: String,
        joiner_name: String,
        /// Hex DER.
        joiner_cert: String,
        /// Hex SPAKE2 message.
        msg: String,
        /// Where the joiner's node-info answers, so the inviter can poll
        /// it; absent from an older node means the default.
        #[serde(default)]
        joiner_info_port: Option<u16>,
    },
    Confirm {
        session: String,
        /// Hex HMAC over the transcript under the agreed key.
        confirm: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PairResponse {
    Started {
        session: String,
        inviter_id: String,
        inviter_name: String,
        inviter_cert: String,
        msg: String,
        cluster_id: String,
        confirm: String,
    },
    Done {
        cluster_id: String,
        members: Vec<Member>,
    },
}

type HmacSha256 = Hmac<Sha256>;

fn confirm_tag(key: &[u8], role: &[u8], transcript: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(role);
    mac.update(b"|");
    mac.update(transcript);
    mac.finalize().into_bytes().to_vec()
}

fn transcript(joiner_fp: &str, inviter_fp: &str, cluster_id: &str) -> Vec<u8> {
    format!("{joiner_fp}|{inviter_fp}|{cluster_id}").into_bytes()
}

fn new_cluster_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// The inviting side, under the lock. Both steps are quick: a SPAKE2
/// finish and an HMAC. `joiner_addr` is where the request came from,
/// which becomes the joiner's card once it is admitted.
pub fn handle_pair(
    r: &mut Router,
    req: PairRequest,
    joiner_addr: Option<std::net::IpAddr>,
) -> Result<PairResponse, (u16, String)> {
    let Some(identity) = r.identity.clone() else {
        return Err((503, "this node has no certificate".into()));
    };
    let self_id = r.self_id.clone();
    let self_name = r.local().map(|n| n.name.clone()).unwrap_or_default();
    let Some(invite) = r.invite.as_mut() else {
        return Err((403, "no invitation is open on this node".into()));
    };
    if invite.expired() {
        r.invite = None;
        return Err((403, "the invitation has expired".into()));
    }
    match req {
        PairRequest::Start {
            joiner_id,
            joiner_name,
            joiner_cert,
            msg,
            joiner_info_port,
        } => {
            let joiner_der = hex::decode(&joiner_cert).map_err(|_| (400, "joiner_cert is not hex".to_string()))?;
            let their_msg = hex::decode(&msg).map_err(|_| (400, "msg is not hex".to_string()))?;
            if joiner_id == self_id {
                return Err((400, "a node cannot pair with itself".into()));
            }
            let (state, our_msg) = Spake2::<Ed25519Group>::start_symmetric(
                &Password::new(invite.pin.as_bytes()),
                &SpakeIdentity::new(SPAKE_ID),
            );
            let key = state
                .finish(&their_msg)
                .map_err(|_| (400, "malformed key exchange message".to_string()))?;
            if r.cluster.cluster_id.is_empty() {
                r.cluster.cluster_id = new_cluster_id();
            }
            let cluster_id = r.cluster.cluster_id.clone();
            let joiner_fp = fingerprint(&joiner_der);
            let transcript = transcript(&joiner_fp, &identity.fingerprint, &cluster_id);
            let confirm = confirm_tag(&key, b"inviter", &transcript);
            let session = hex::encode(rand::random::<[u8; 8]>());
            let invite = r.invite.as_mut().expect("checked above");
            invite.sessions.insert(
                session.clone(),
                Session {
                    key,
                    joiner: Member {
                        id: joiner_id,
                        name: joiner_name,
                        fingerprint: joiner_fp,
                        added_ms: crate::status::now_ms(),
                    },
                    transcript,
                    joiner_addr,
                    joiner_info_port: joiner_info_port.unwrap_or_else(node_info_port),
                },
            );
            Ok(PairResponse::Started {
                session,
                inviter_id: self_id,
                inviter_name: self_name,
                inviter_cert: hex::encode(identity.cert.as_ref()),
                msg: hex::encode(our_msg),
                cluster_id,
                confirm: hex::encode(confirm),
            })
        }
        PairRequest::Confirm { session, confirm } => {
            let Some(s) = invite.sessions.get(&session).cloned() else {
                return Err((404, "unknown pairing session".into()));
            };
            let expected = confirm_tag(&s.key, b"joiner", &s.transcript);
            let given = hex::decode(&confirm).unwrap_or_default();
            if !constant_eq(&expected, &given) {
                invite.wrong_attempts += 1;
                invite.sessions.remove(&session);
                let left = INVITE_ATTEMPTS.saturating_sub(invite.wrong_attempts);
                if left == 0 {
                    r.invite = None;
                    r.push_log("invitation closed after three wrong PINs");
                    return Err((403, "wrong PIN; the invitation is now closed".into()));
                }
                return Err((403, format!("wrong PIN ({left} attempt(s) left)")));
            }
            invite.sessions.remove(&session);
            let name = s.joiner.name.clone();
            let joiner_id = s.joiner.id.clone();
            r.admit(s.joiner);
            // A card for the joiner at the address it paired from, polled
            // at once — discovery may or may not have seen it (multicast
            // is lossy, and two nodes on one host never see each other).
            if let Some(ip) = s.joiner_addr {
                let mut node = super::Node::new_peer(
                    joiner_id.clone(),
                    name.clone(),
                    ip.to_string(),
                    super::Source::Manual,
                    Instant::now(),
                );
                node.info_port = s.joiner_info_port;
                match r.nodes.get_mut(&joiner_id) {
                    Some(existing) => {
                        existing.address = ip.to_string();
                        existing.info_port = s.joiner_info_port;
                        existing.next_poll = Instant::now();
                    }
                    None => r.upsert_peer(node),
                }
            }
            r.push_log(format!("paired with {name}"));
            let mut members: Vec<Member> = r.cluster.members.values().cloned().collect();
            members.push(Member {
                id: self_id,
                name: self_name,
                fingerprint: identity.fingerprint.clone(),
                added_ms: crate::status::now_ms(),
            });
            Ok(PairResponse::Done {
                cluster_id: r.cluster.cluster_id.clone(),
                members,
            })
        }
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Where a peer's pairing endpoint lives, from what the operator typed.
pub fn pair_url(address: &str) -> String {
    let addr = address.trim().trim_end_matches('/');
    if addr.contains(':') && !addr.starts_with('[') {
        format!("http://{addr}/v1/pair")
    } else {
        format!("http://{addr}:{}/v1/pair", node_info_port())
    }
}

/// The joining side: run the two steps against `address` with `pin`.
/// Returns what to tell the operator.
pub async fn join(shared: Shared, address: String, pin: String) -> Result<String, String> {
    let pin = pin.trim().to_string();
    if pin.len() != 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
        return Err("the PIN is six digits".into());
    }
    let (identity, self_id, self_name) = {
        let r = lock(&shared);
        let Some(identity) = r.identity.clone() else {
            return Err("this node has no certificate".into());
        };
        (identity, r.self_id.clone(), r.local().map(|n| n.name.clone()).unwrap_or_default())
    };
    let (state, our_msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(pin.as_bytes()),
        &SpakeIdentity::new(SPAKE_ID),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let url = pair_url(&address);
    let started: PairResponse = post(&client, &url, &PairRequest::Start {
        joiner_id: self_id.clone(),
        joiner_name: self_name.clone(),
        joiner_cert: hex::encode(identity.cert.as_ref()),
        msg: hex::encode(our_msg),
        joiner_info_port: Some(node_info_port()),
    })
    .await?;
    let PairResponse::Started {
        session,
        inviter_id,
        inviter_name,
        inviter_cert,
        msg,
        cluster_id,
        confirm,
    } = started
    else {
        return Err("unexpected answer to the first pairing step".into());
    };
    let their_msg = hex::decode(&msg).map_err(|_| "malformed key exchange message".to_string())?;
    let inviter_der = hex::decode(&inviter_cert).map_err(|_| "malformed certificate".to_string())?;
    let key = state
        .finish(&their_msg)
        .map_err(|_| "malformed key exchange message".to_string())?;
    let inviter_fp = fingerprint(&inviter_der);
    let transcript = transcript(&identity.fingerprint, &inviter_fp, &cluster_id);
    let expected = confirm_tag(&key, b"inviter", &transcript);
    if !constant_eq(&expected, &hex::decode(&confirm).unwrap_or_default()) {
        return Err("the PIN did not match (or something on the network interfered)".into());
    }
    {
        let r = lock(&shared);
        if r.cluster.is_clustered() && r.cluster.cluster_id != cluster_id {
            return Err("this node is already in a different cluster; leave it first".into());
        }
    }
    let done: PairResponse = post(&client, &url, &PairRequest::Confirm {
        session,
        confirm: hex::encode(confirm_tag(&key, b"joiner", &transcript)),
    })
    .await?;
    let PairResponse::Done { cluster_id, members } = done else {
        return Err("unexpected answer to the confirmation step".into());
    };
    let mut r = lock(&shared);
    r.cluster.cluster_id = cluster_id;
    r.admit(Member {
        id: inviter_id.clone(),
        name: inviter_name.clone(),
        fingerprint: inviter_fp,
        added_ms: crate::status::now_ms(),
    });
    let mut added = 0;
    for m in members {
        if m.id != self_id && !r.cluster.members.contains_key(&m.id) {
            r.admit(m);
            added += 1;
        }
    }
    // The inviter is reachable at the address that was typed; a card for
    // it starts polling at once.
    let (host, port) = super::Node::parse_address(&address);
    match r.nodes.get_mut(&inviter_id) {
        Some(existing) => {
            existing.address = host;
            existing.info_port = port;
            existing.next_poll = Instant::now();
        }
        None => {
            let mut node = super::Node::new_peer(
                inviter_id.clone(),
                inviter_name.clone(),
                host,
                super::Source::Manual,
                Instant::now(),
            );
            node.info_port = port;
            r.upsert_peer(node);
        }
    }
    let size = r.cluster.members.len() + 1;
    r.push_log(format!("paired with {inviter_name}; cluster of {size} node(s)"));
    Ok(format!(
        "paired with {inviter_name}{}",
        if added > 0 { format!(" and {added} other member(s)") } else { String::new() }
    ))
}

async fn post(client: &reqwest::Client, url: &str, req: &PairRequest) -> Result<PairResponse, String> {
    let resp = client
        .post(url)
        .json(req)
        .send()
        .await
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let msg = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).to_string());
        return Err(format!("{status}: {msg}"));
    }
    serde_json::from_slice(&body).map_err(|e| format!("malformed pairing answer: {e}"))
}

// ------------------------------------------------------------------ the router's side

impl Router {
    /// Pin a member and make the TLS side see it at once.
    pub fn admit(&mut self, member: Member) {
        self.cluster.removed.remove(&member.id);
        self.cluster.members.insert(member.id.clone(), member);
        self.refresh_trust();
    }

    /// Drop a member for good: unpinned here, and a tombstone so a
    /// remaining member's report does not add it back.
    pub fn remove_member(&mut self, id: &str) {
        if self.cluster.members.remove(id).is_some() {
            self.cluster.removed.insert(id.to_string(), crate::status::now_ms());
            self.refresh_trust();
        }
    }

    pub fn leave_cluster(&mut self) {
        self.cluster = ClusterFile::default();
        self.invite = None;
        self.refresh_trust();
    }

    pub fn is_member(&self, id: &str) -> bool {
        self.cluster.members.contains_key(id)
    }

    /// Members another member reports: accepted only from a report that
    /// arrived over mutual TLS (the caller checks), for the same cluster,
    /// skipping tombstones. That is how a cluster stays symmetric without a
    /// primary: everyone pins everyone.
    pub fn merge_members(&mut self, cluster_id: &str, members: Vec<Member>) -> usize {
        if cluster_id.is_empty() || cluster_id != self.cluster.cluster_id {
            return 0;
        }
        let mut added = 0;
        for m in members {
            if m.id == self.self_id
                || m.fingerprint.is_empty()
                || self.cluster.members.contains_key(&m.id)
                || self.cluster.removed.contains_key(&m.id)
            {
                continue;
            }
            self.cluster.members.insert(m.id.clone(), m);
            added += 1;
        }
        if added > 0 {
            self.refresh_trust();
        }
        added
    }

    /// Push the member list into the live pin set and persist the file.
    pub fn refresh_trust(&mut self) {
        self.pins.set(self.cluster.fingerprints());
        if let Err(e) = self.cluster.save(&self.node_dir) {
            self.push_log(format!("cannot save cluster.json: {e}"));
        }
    }

    /// Open an invitation, replacing any earlier one.
    pub fn open_invite(&mut self) -> String {
        if self.cluster.cluster_id.is_empty() {
            self.cluster.cluster_id = new_cluster_id();
            self.refresh_trust();
        }
        let inv = Invite::new();
        let pin = inv.pin.clone();
        self.invite = Some(inv);
        self.push_log("invitation open for five minutes");
        pin
    }

    pub fn members_report(&self) -> Vec<Member> {
        self.cluster.members.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{new_shared_for_tests, Source};

    fn party(name: &str) -> (Shared, Arc<Identity>) {
        let dir = std::env::temp_dir().join(format!(
            "kmplify-cluster-test-{name}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let shared = new_shared_for_tests(name);
        let identity = Arc::new(Identity::load_or_create(&dir, name).expect("identity"));
        {
            let mut r = lock(&shared);
            r.node_dir = dir;
            r.identity = Some(identity.clone());
            if let Some(n) = r.local_mut() {
                n.name = name.to_string();
            }
        }
        (shared, identity)
    }

    /// The two ends of the protocol in one process, no network: the
    /// joiner's steps are what `join` sends, the inviter's what
    /// `handle_pair` answers.
    fn run_pairing(inviter: &Shared, joiner: &Shared, joiner_id: &Identity, pin: &str) -> Result<PairResponse, (u16, String)> {
        let (state, msg) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(pin.as_bytes()),
            &SpakeIdentity::new(SPAKE_ID),
        );
        let joiner_node_id = lock(joiner).self_id.clone();
        let started = handle_pair(
            &mut lock(inviter),
            PairRequest::Start {
                joiner_id: joiner_node_id.clone(),
                joiner_name: "joiner".into(),
                joiner_cert: hex::encode(joiner_id.cert.as_ref()),
                msg: hex::encode(msg),
                joiner_info_port: Some(24418),
            },
            Some("10.0.0.9".parse().unwrap()),
        )?;
        let PairResponse::Started { session, inviter_cert, msg, cluster_id, confirm, .. } = started else {
            panic!("expected Started");
        };
        let key = state.finish(&hex::decode(msg).unwrap()).unwrap();
        let inviter_fp = fingerprint(&hex::decode(inviter_cert).unwrap());
        let t = transcript(&joiner_id.fingerprint, &inviter_fp, &cluster_id);
        assert!(
            constant_eq(&confirm_tag(&key, b"inviter", &t), &hex::decode(&confirm).unwrap()),
            "the inviter's confirmation must verify under the joiner's key"
        );
        handle_pair(
            &mut lock(inviter),
            PairRequest::Confirm {
                session,
                confirm: hex::encode(confirm_tag(&key, b"joiner", &t)),
            },
            None,
        )
    }

    #[test]
    fn an_identity_is_minted_once_and_reloaded_after() {
        let dir = std::env::temp_dir().join(format!("kmplify-id-test-{}-{}", std::process::id(), rand::random::<u32>()));
        let a = Identity::load_or_create(&dir, "abcdef0123456789").unwrap();
        let b = Identity::load_or_create(&dir, "abcdef0123456789").unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "a second start must not change the fingerprint peers pinned");
        assert_eq!(a.fingerprint.len(), 64);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pairing_with_the_right_pin_pins_both_sides() {
        let (inviter, inviter_id) = party("inviter");
        let (joiner, joiner_id) = party("joiner");
        let pin = lock(&inviter).open_invite();
        let done = run_pairing(&inviter, &joiner, &joiner_id, &pin).expect("pairing");
        let PairResponse::Done { cluster_id, members } = done else { panic!("expected Done") };
        assert!(!cluster_id.is_empty());
        assert!(members.iter().any(|m| m.fingerprint == inviter_id.fingerprint));
        let r = lock(&inviter);
        assert!(r.is_member("joiner"));
        assert!(r.pins.contains(&joiner_id.fingerprint));
        assert_eq!(r.cluster.members["joiner"].fingerprint, joiner_id.fingerprint);
        let card = &r.nodes["joiner"];
        assert_eq!(card.address, "10.0.0.9", "the joiner gets a card at the address it paired from");
        assert_eq!(card.info_port, 24418);
    }

    #[test]
    fn a_wrong_pin_is_refused_and_three_of_them_close_the_invite() {
        let (inviter, _) = party("inviter2");
        let (joiner, joiner_id) = party("joiner2");
        let pin = lock(&inviter).open_invite();
        let wrong = if pin == "000000" { "000001" } else { "000000" };
        for attempt in 1..=3 {
            let (state, msg) = Spake2::<Ed25519Group>::start_symmetric(
                &Password::new(wrong.as_bytes()),
                &SpakeIdentity::new(SPAKE_ID),
            );
            let started = handle_pair(
                &mut lock(&inviter),
                PairRequest::Start {
                    joiner_id: "joiner2".into(),
                    joiner_name: "j".into(),
                    joiner_cert: hex::encode(joiner_id.cert.as_ref()),
                    msg: hex::encode(msg),
                    joiner_info_port: None,
                },
                None,
            );
            let Ok(PairResponse::Started { session, msg, inviter_cert, cluster_id, confirm, .. }) = started else {
                panic!("start is answered even for a wrong PIN: the PIN is checked at confirmation");
            };
            let key = state.finish(&hex::decode(msg).unwrap()).unwrap();
            let inviter_fp = fingerprint(&hex::decode(inviter_cert).unwrap());
            let t = transcript(&joiner_id.fingerprint, &inviter_fp, &cluster_id);
            assert!(
                !constant_eq(&confirm_tag(&key, b"inviter", &t), &hex::decode(&confirm).unwrap()),
                "a joiner with the wrong PIN cannot verify the inviter either"
            );
            let err = handle_pair(
                &mut lock(&inviter),
                PairRequest::Confirm { session, confirm: hex::encode(confirm_tag(&key, b"joiner", &t)) },
                None,
            )
            .unwrap_err();
            assert_eq!(err.0, 403, "attempt {attempt}");
        }
        let r = lock(&inviter);
        assert!(r.invite.is_none(), "closed after three wrong PINs");
        assert!(!r.is_member("joiner2"));
        drop(r);
        let _ = joiner;
    }

    #[test]
    fn no_open_invite_means_no_pairing() {
        let (inviter, _) = party("inviter3");
        let err = handle_pair(
            &mut lock(&inviter),
            PairRequest::Start {
                joiner_id: "x".into(),
                joiner_name: "x".into(),
                joiner_cert: "00".into(),
                msg: "00".into(),
                joiner_info_port: None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(err.0, 403);
    }

    #[test]
    fn member_reports_merge_only_for_the_same_cluster_and_never_resurrect_removed_nodes() {
        let (shared, _) = party("merge");
        let mut r = lock(&shared);
        r.cluster.cluster_id = "c1".into();
        let m = |id: &str| Member { id: id.into(), name: id.into(), fingerprint: format!("fp-{id}"), added_ms: 0 };
        assert_eq!(r.merge_members("other", vec![m("a")]), 0);
        assert_eq!(r.merge_members("c1", vec![m("a"), m("b")]), 2);
        r.remove_member("a");
        assert_eq!(r.merge_members("c1", vec![m("a")]), 0, "tombstoned");
        assert!(r.pins.contains("fp-b"));
        assert!(!r.pins.contains("fp-a"));
        r.admit(m("a"));
        assert!(r.pins.contains("fp-a"), "pairing again clears the tombstone");
    }

    #[test]
    fn a_pinned_certificate_passes_the_verifier_and_a_stranger_does_not() {
        let (_, id) = party("verify");
        let pins = Pins::default();
        let v = PinnedVerifier { pins: pins.clone(), provider: provider() };
        let now = UnixTime::now();
        assert!(v.verify_client_cert(&id.cert, &[], now).is_err());
        pins.set(HashSet::from([id.fingerprint.clone()]));
        assert!(v.verify_client_cert(&id.cert, &[], now).is_ok());
        assert!(server_config(&id, &pins).is_ok());
        assert!(client_config(&id, &pins).is_ok());
    }

    #[test]
    fn pair_url_respects_a_typed_port() {
        assert_eq!(pair_url("10.0.0.5"), "http://10.0.0.5:14418/v1/pair");
        assert_eq!(pair_url("10.0.0.5:9000/"), "http://10.0.0.5:9000/v1/pair");
    }

    #[test]
    fn leaving_forgets_everything() {
        let (shared, _) = party("leave");
        let mut r = lock(&shared);
        r.open_invite();
        r.admit(Member { id: "p".into(), name: "p".into(), fingerprint: "f".into(), added_ms: 0 });
        r.leave_cluster();
        assert!(!r.cluster.is_clustered());
        assert!(r.invite.is_none());
        assert_eq!(r.pins.count(), 0);
        let _ = Source::Local;
    }
}
