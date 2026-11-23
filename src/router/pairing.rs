//! PIN pairing: a second device gets the cluster key without anybody
//! showing, copying or typing it (router protocol v2, owner decision
//! 2026-11-23 "Show key: no").
//!
//! The device that holds the key (or makes one) opens an **invitation**: a
//! six-digit PIN, good for [`PIN_TTL`] and for at most [`MAX_WRONG`] wrong
//! PINs. The operator reads the PIN off that screen and enters it on the
//! joining device. The two run SPAKE2 with the PIN as the password; the
//! session key it gives them seals the cluster key on its way across, and
//! both confirm the key before anything is handed over.
//!
//! ```text
//! joiner                                         inviter (holds the key)
//!   A = SPAKE2.start_a(PIN)
//!   POST /v2/pair/start {v, msg: A, name}   ->
//!                                                B = SPAKE2.start_b(PIN)
//!                                                K = finish(A)
//!                          <-  {v, session, msg: B}
//!   K = finish(B)
//!   th = SHA-256(label | A | B | session)
//!   kj, ki, ks = HKDF-SHA256(salt th, ikm K)
//!   POST /v2/pair/finish {session, confirm: HMAC(kj, th)}  ->
//!                                                wrong: one of MAX_WRONG used,
//!                                                       nothing else said
//!                                                right: invitation closes
//!       <- {confirm: HMAC(ki, th), fingerprint, name,
//!           nonce, sealed: AES-256-GCM(ks, nonce, cluster key,
//!                                      aad = th | fingerprint)}
//!   check ki's confirmation, open, check the key's fingerprint
//! ```
//!
//! What this gives, and why in this order:
//!
//! * a listener on the wire learns nothing it can test PINs against offline
//!   (SPAKE2's property);
//! * an active attacker, on either side, gets **one guess per session**, and
//!   a wrong guess against the inviter costs one of the invitation's three
//!   attempts: the joiner confirms first, so the inviter learns of a wrong
//!   PIN before it says anything that depends on the PIN. Starting sessions
//!   and never finishing them gives nothing to test a guess with;
//! * the cluster key crosses only sealed with a key derived from the PIN
//!   exchange, and only after the joiner proved it knows the PIN;
//! * both screens then show the same cluster key fingerprint for the
//!   operator to compare.
//!
//! What it does not do: an attacker on the allowlist can spend the three
//! attempts and void a PIN (a denial of service; open a new one). The PIN is
//! held in memory only, by the process that shows it, and never logged or
//! written to a file. Pairing listens on [`policy::bind_addr`]'s rules: on
//! loopback, or for the [`PIN_TTL`] of an invitation on the LAN for the
//! allowlist only ("pairing mode"); it never announces itself by mDNS.
//!
//! The PAKE is the RustCrypto `spake2` crate (Ed25519 group). It has NOT had
//! an independent audit (its README says so); see docs/ROUTER.md.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use super::key::{hex, unhex, ClusterKey, KeyStore};
use super::policy::{self, Allowlist, Mode};
use crate::dashboard::http::{self, Request, Response};

type HmacSha256 = Hmac<Sha256>;

/// Protocol version of the pairing messages.
pub const PAIR_VERSION: u32 = 2;
/// How long a PIN is good for.
pub const PIN_TTL: Duration = Duration::from_secs(120);
pub const PIN_TTL_MS: u64 = 120_000;
/// Wrong PINs an invitation takes before it is void.
pub const MAX_WRONG: u32 = 3;
/// Sessions started and not finished, kept at most (the oldest goes).
const MAX_PENDING: usize = 4;
pub const START_PATH: &str = "/v2/pair/start";
pub const FINISH_PATH: &str = "/v2/pair/finish";
/// The SPAKE2 identities: fixed, so both sides agree without sending them.
const ID_JOINER: &[u8] = b"kmplify-router-v2/pair/joiner";
const ID_INVITER: &[u8] = b"kmplify-router-v2/pair/inviter";
const TRANSCRIPT_LABEL: &[u8] = b"kmplify-router-v2/pair/transcript\n";

/// The pairing port when `KMPLIFY_ROUTER_PAIR_PORT` does not say: the
/// node-info port plus one (14419 by default).
pub fn pair_port() -> u16 {
    std::env::var("KMPLIFY_ROUTER_PAIR_PORT")
        .ok()
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or_else(|| {
            std::env::var("KMPLIFY_ROUTER_PORT")
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok())
                .filter(|p| *p != 0 && *p != u16::MAX)
                .unwrap_or(super::NODE_INFO_PORT)
                + 1
        })
}

/// A fresh six-digit PIN, uniform over 000000..=999999.
pub fn new_pin() -> String {
    use rand_core::RngCore;
    // The largest multiple of a million below 2^32: rejection keeps every
    // PIN equally likely.
    const LIMIT: u32 = 4_294_000_000;
    loop {
        let n = rand_core::OsRng.next_u32();
        if n < LIMIT {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

/// A PIN as typed: six digits, spaces and dashes ignored.
pub fn parse_pin(s: &str) -> Result<String, String> {
    let digits: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(digits)
    } else {
        Err("the PIN is the six digits the other device shows".into())
    }
}

// ------------------------------------------------------------ key schedule

fn transcript(a: &[u8], b: &[u8], session: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(TRANSCRIPT_LABEL);
    for part in [a, b, session.as_bytes()] {
        h.update((part.len() as u16).to_be_bytes());
        h.update(part);
    }
    h.finalize().into()
}

struct Keys {
    joiner_confirm: [u8; 32],
    inviter_confirm: [u8; 32],
    seal: [u8; 32],
}

fn schedule(k: &[u8], th: &[u8; 32]) -> Keys {
    let hk = hkdf::Hkdf::<Sha256>::new(Some(th), k);
    let mut out = Keys {
        joiner_confirm: [0; 32],
        inviter_confirm: [0; 32],
        seal: [0; 32],
    };
    // 32 bytes is far below HKDF-SHA256's limit: never an error.
    hk.expand(b"kmplify-router-v2 confirm joiner", &mut out.joiner_confirm)
        .expect("hkdf length");
    hk.expand(
        b"kmplify-router-v2 confirm inviter",
        &mut out.inviter_confirm,
    )
    .expect("hkdf length");
    hk.expand(b"kmplify-router-v2 seal cluster key", &mut out.seal)
        .expect("hkdf length");
    out
}

fn confirm(key: &[u8; 32], th: &[u8; 32]) -> String {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(th);
    hex(&m.finalize().into_bytes())
}

fn confirm_ok(key: &[u8; 32], th: &[u8; 32], given_hex: &str) -> bool {
    let Some(given) = unhex(given_hex) else {
        return false;
    };
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(th);
    m.verify_slice(&given).is_ok()
}

fn aad(th: &[u8; 32], fingerprint: &str) -> Vec<u8> {
    [th.as_slice(), b"|", fingerprint.as_bytes()].concat()
}

// ------------------------------------------------------------ messages

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartReq {
    pub v: u32,
    pub msg: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartResp {
    pub v: u32,
    pub session: String,
    pub msg: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishReq {
    pub session: String,
    pub confirm: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishResp {
    pub confirm: String,
    pub fingerprint: String,
    pub name: String,
    pub nonce: String,
    pub sealed: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Refused {
    pub error: String,
    #[serde(default)]
    pub attempts_left: u32,
}

// ------------------------------------------------------------ inviter

/// Where an invitation stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    /// No invitation in this process.
    #[default]
    None,
    /// The PIN is on screen, waiting for the other device.
    Waiting,
    /// A device joined; the invitation is closed.
    Paired,
    /// Three wrong PINs: the invitation is void.
    Void,
    /// The two minutes ran out.
    Expired,
    /// Closed by the operator.
    Closed,
    /// It could not listen.
    Failed,
}

impl PairState {
    pub fn label(self) -> &'static str {
        match self {
            PairState::None => "no pairing open",
            PairState::Waiting => "waiting for the other device",
            PairState::Paired => "paired",
            PairState::Void => "void after three wrong PINs",
            PairState::Expired => "expired",
            PairState::Closed => "closed",
            PairState::Failed => "could not open",
        }
    }
}

struct Pending {
    th: [u8; 32],
    keys: Keys,
    name: String,
}

/// The inviting side's state machine, without any network: the listener
/// ([`Host`]) and the tests drive it. `now_ms` comes in from outside so the
/// tests hold the clock.
pub struct Inviter {
    pin: String,
    key: ClusterKey,
    name: String,
    opened_ms: u64,
    wrong: u32,
    state: PairState,
    pending: Vec<(String, Pending)>,
    joined: String,
}

impl std::fmt::Debug for Inviter {
    // Never the PIN or the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inviter")
            .field("state", &self.state)
            .field("wrong", &self.wrong)
            .finish()
    }
}

impl Inviter {
    pub fn new(pin: String, key: ClusterKey, name: String, now_ms: u64) -> Self {
        Self {
            pin,
            key,
            name,
            opened_ms: now_ms,
            wrong: 0,
            state: PairState::Waiting,
            pending: Vec::new(),
            joined: String::new(),
        }
    }

    pub fn state(&mut self, now_ms: u64) -> PairState {
        if self.state == PairState::Waiting && now_ms.saturating_sub(self.opened_ms) >= PIN_TTL_MS {
            self.state = PairState::Expired;
            self.pending.clear();
        }
        self.state
    }

    pub fn expires_ms(&self) -> u64 {
        self.opened_ms + PIN_TTL_MS
    }

    pub fn attempts_left(&self) -> u32 {
        MAX_WRONG.saturating_sub(self.wrong)
    }

    pub fn joined(&self) -> &str {
        &self.joined
    }

    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn close(&mut self) {
        if self.state == PairState::Waiting {
            self.state = PairState::Closed;
        }
        self.pending.clear();
    }

    fn refused(&self, error: &str) -> Refused {
        Refused {
            error: error.into(),
            attempts_left: self.attempts_left(),
        }
    }

    fn open_or_refuse(&mut self, now_ms: u64) -> Result<(), (u16, Refused)> {
        match self.state(now_ms) {
            PairState::Waiting => Ok(()),
            PairState::Void => Err((403, self.refused("this PIN is void after three wrong PINs"))),
            PairState::Expired => Err((403, self.refused("this PIN has expired"))),
            _ => Err((403, self.refused("no pairing is open on that device"))),
        }
    }

    /// Step one: the joiner's SPAKE2 message in, ours out.
    pub fn start(&mut self, req: &StartReq, now_ms: u64) -> Result<StartResp, (u16, Refused)> {
        self.open_or_refuse(now_ms)?;
        if req.v != PAIR_VERSION {
            return Err((400, self.refused("unsupported pairing version")));
        }
        let a = unhex(&req.msg)
            .filter(|m| m.len() == 33)
            .ok_or_else(|| (400, self.refused("malformed pairing message")))?;
        let (spake, b) = Spake2::<Ed25519Group>::start_b(
            &Password::new(self.pin.as_bytes()),
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        let k = spake
            .finish(&a)
            .map_err(|_| (400, self.refused("malformed pairing message")))?;
        let session = super::auth::new_nonce();
        let th = transcript(&a, &b, &session);
        let name = policy::check_name(&req.name).unwrap_or_else(|_| "a device".into());
        if self.pending.len() >= MAX_PENDING {
            self.pending.remove(0);
        }
        self.pending.push((
            session.clone(),
            Pending {
                th,
                keys: schedule(&k, &th),
                name,
            },
        ));
        Ok(StartResp {
            v: PAIR_VERSION,
            session,
            msg: hex(&b),
        })
    }

    /// Step two: the joiner's confirmation in; on a right PIN the sealed
    /// cluster key out, and the invitation closes.
    pub fn finish(&mut self, req: &FinishReq, now_ms: u64) -> Result<FinishResp, (u16, Refused)> {
        self.open_or_refuse(now_ms)?;
        let Some(i) = self.pending.iter().position(|(s, _)| *s == req.session) else {
            return Err((404, self.refused("unknown pairing session")));
        };
        let (_, p) = self.pending.remove(i);
        if !confirm_ok(&p.keys.joiner_confirm, &p.th, &req.confirm) {
            self.wrong += 1;
            if self.wrong >= MAX_WRONG {
                self.state = PairState::Void;
                self.pending.clear();
                return Err((403, self.refused("wrong PIN; this PIN is now void")));
            }
            return Err((403, self.refused("wrong PIN")));
        }
        let fingerprint = self.key.fingerprint();
        let mut nonce = [0u8; 12];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
        let cipher = Aes256Gcm::new_from_slice(&p.keys.seal).expect("32 byte key");
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: self.key.bytes(),
                    aad: &aad(&p.th, &fingerprint),
                },
            )
            .map_err(|_| (500, self.refused("could not seal")))?;
        self.state = PairState::Paired;
        self.joined = p.name;
        self.pending.clear();
        Ok(FinishResp {
            confirm: confirm(&p.keys.inviter_confirm, &p.th),
            fingerprint,
            name: self.name.clone(),
            nonce: hex(&nonce),
            sealed: hex(&sealed),
        })
    }

    /// One HTTP request to the pairing listener.
    pub fn answer(&mut self, req: &Request, now_ms: u64) -> Response {
        if req.method != "POST" {
            return Response::text(405, "POST only");
        }
        fn reply<T: Serialize>(r: Result<T, (u16, Refused)>) -> Response {
            match r {
                Ok(v) => Response::json(200, &serde_json::json!(v)),
                Err((code, why)) => Response::json(code, &serde_json::json!(why)),
            }
        }
        match req.path.as_str() {
            START_PATH => match serde_json::from_slice::<StartReq>(&req.body) {
                Ok(s) => reply(self.start(&s, now_ms)),
                Err(_) => Response::text(400, "bad request"),
            },
            FINISH_PATH => match serde_json::from_slice::<FinishReq>(&req.body) {
                Ok(f) => reply(self.finish(&f, now_ms)),
                Err(_) => Response::text(400, "bad request"),
            },
            _ => Response::text(404, "not found"),
        }
    }
}

// ------------------------------------------------------------ joiner

/// The joining side's first step: its SPAKE2 state and the request.
pub struct JoinStart {
    spake: Spake2<Ed25519Group>,
    a: Vec<u8>,
    pub request: StartReq,
}

impl JoinStart {
    pub fn new(pin: &str, my_name: &str) -> Self {
        let (spake, a) = Spake2::<Ed25519Group>::start_a(
            &Password::new(pin.as_bytes()),
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        Self {
            spake,
            request: StartReq {
                v: PAIR_VERSION,
                msg: hex(&a),
                name: my_name.into(),
            },
            a,
        }
    }

    /// The inviter's answer in, the confirmation to send out.
    pub fn step(self, resp: &StartResp) -> Result<JoinFinish, String> {
        if resp.v != PAIR_VERSION {
            return Err("the other device speaks another pairing version".into());
        }
        let b = unhex(&resp.msg)
            .filter(|m| m.len() == 33)
            .ok_or("the other device sent a malformed pairing message")?;
        let k = self
            .spake
            .finish(&b)
            .map_err(|_| "the other device sent a malformed pairing message")?;
        let th = transcript(&self.a, &b, &resp.session);
        let keys = schedule(&k, &th);
        Ok(JoinFinish {
            request: FinishReq {
                session: resp.session.clone(),
                confirm: confirm(&keys.joiner_confirm, &th),
            },
            th,
            keys,
        })
    }
}

pub struct JoinFinish {
    th: [u8; 32],
    keys: Keys,
    pub request: FinishReq,
}

/// What a successful join brought.
pub struct Joined {
    pub key: ClusterKey,
    pub fingerprint: String,
    pub inviter_name: String,
}

impl std::fmt::Debug for Joined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Joined({})", self.fingerprint)
    }
}

impl JoinFinish {
    /// The inviter's final answer in: its confirmation checked, the key
    /// opened and checked against the fingerprint it announced.
    pub fn open(self, resp: &FinishResp) -> Result<Joined, String> {
        if !confirm_ok(&self.keys.inviter_confirm, &self.th, &resp.confirm) {
            return Err("the other device could not prove it knows the PIN".into());
        }
        let nonce = unhex(&resp.nonce)
            .filter(|n| n.len() == 12)
            .ok_or("malformed answer")?;
        let sealed = unhex(&resp.sealed).ok_or("malformed answer")?;
        let cipher = Aes256Gcm::new_from_slice(&self.keys.seal).expect("32 byte key");
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &sealed,
                    aad: &aad(&self.th, &resp.fingerprint),
                },
            )
            .map_err(|_| "the sealed key did not open")?;
        let bytes: [u8; 32] = plain
            .as_slice()
            .try_into()
            .map_err(|_| "the sealed key has the wrong length")?;
        let key = ClusterKey::from_array(bytes);
        if key.fingerprint() != resp.fingerprint {
            return Err("the key does not match the fingerprint the other device shows".into());
        }
        Ok(Joined {
            fingerprint: key.fingerprint(),
            key,
            inviter_name: policy::check_name(&resp.name).unwrap_or_else(|_| "a device".into()),
        })
    }
}

/// `host[:port]` as typed on the joining device: an IP address (a pairing
/// never resolves names), on loopback or on this node's allowlist.
pub fn parse_target(at: &str, allow: &Allowlist) -> Result<SocketAddr, String> {
    let at = at.trim();
    let addr: SocketAddr = match at.parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => match at.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
            Ok(ip) => SocketAddr::new(ip, pair_port()),
            Err(_) => {
                return Err(format!(
                    "{at:?} is not an address; enter the address the other device shows, \
                     like 192.168.1.20 or 192.168.1.20:{}",
                    pair_port()
                ))
            }
        },
    };
    let ip = policy::canonical(addr.ip());
    if ip.is_loopback() || allow.allows(ip) {
        Ok(SocketAddr::new(ip, addr.port()))
    } else {
        Err(format!(
            "{ip} is neither this computer nor on this node's allowlist; set router-allow \
             (for example 192.168.1.0/24) to pair over the network"
        ))
    }
}

async fn post<T: Serialize, R: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
    body: &T,
) -> Result<R, String> {
    let resp = client.post(url).json(body).send().await.map_err(|e| {
        if e.is_timeout() {
            "the other device did not answer in time".to_string()
        } else {
            "cannot reach the other device (is its pairing open, and the address right?)"
                .to_string()
        }
    })?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|_| "the answer broke off")?;
    if bytes.len() > 8 * 1024 {
        return Err("the answer is too large".into());
    }
    if status != 200 {
        let why = serde_json::from_slice::<Refused>(&bytes)
            .map(|r| {
                if r.error == "wrong PIN" {
                    format!(
                        "wrong PIN ({} attempt(s) left on that PIN)",
                        r.attempts_left
                    )
                } else {
                    r.error
                }
            })
            .unwrap_or_else(|_| format!("the other device answered {status}"));
        return Err(why);
    }
    serde_json::from_slice(&bytes).map_err(|_| "the other device sent a malformed answer".into())
}

/// Join the cluster of the device at `target` with `pin`. One attempt.
pub async fn join(target: SocketAddr, pin: &str, my_name: &str) -> Result<Joined, String> {
    let pin = parse_pin(pin)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let base = format!("http://{target}");
    let start = JoinStart::new(&pin, my_name);
    let started: StartResp = post(&client, &format!("{base}{START_PATH}"), &start.request).await?;
    let finish = start.step(&started)?;
    let done: FinishResp = post(&client, &format!("{base}{FINISH_PATH}"), &finish.request).await?;
    finish.open(&done)
}

/// [`join`] for a caller without a runtime of its own (a view's button, the
/// CLI): runs on a thread of its own, stores the key in `keys`.
pub fn join_blocking(
    node_dir: &std::path::Path,
    keys: &dyn KeyStore,
    at: &str,
    pin: &str,
) -> Result<Joined, String> {
    let config = config_for(node_dir);
    let target = parse_target(at, &config.allow)?;
    let pin = parse_pin(pin)?;
    let name = config.name.clone();
    let joined = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(join(target, &pin, &name))
    })
    .join()
    .map_err(|_| "the pairing thread stopped".to_string())??;
    keys.save(&joined.key)?;
    Ok(joined)
}

fn config_for(node_dir: &std::path::Path) -> super::Config {
    let stored = crate::settings::Settings::load(node_dir);
    let node_id = crate::status::read_published(node_dir)
        .map(|s| s.node_id)
        .unwrap_or_default();
    super::Config::resolve(&stored, &node_id, true)
}

// ------------------------------------------------------------ the listener

/// Where an invitation listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// 127.0.0.1: a device on this computer (a desktop beside a node).
    ThisComputer,
    /// Every interface, for the allowlist only, for the PIN's two minutes.
    Network,
}

/// What a view shows about this process's invitation. The PIN is in it
/// while it waits: a view shows it, nothing stores it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PairView {
    pub state: PairState,
    pub state_label: String,
    pub pin: String,
    pub seconds_left: u64,
    pub attempts_left: u32,
    pub scope: String,
    /// `address:port` the other device enters.
    pub addresses: Vec<String>,
    pub fingerprint: String,
    pub joined: String,
    pub message: String,
}

struct HostInner {
    inviter: Option<Inviter>,
    scope: Scope,
    addresses: Vec<String>,
    fingerprint: String,
    message: String,
    failed: bool,
}

/// An open invitation with its listener, owned by the process that shows the
/// PIN. Dropping it closes the listener.
pub struct Host {
    inner: Arc<Mutex<HostInner>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Host(..)")
    }
}

fn lock(m: &Mutex<HostInner>) -> std::sync::MutexGuard<'_, HostInner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The scope an invitation gets: the network only with an allowlist and
/// the router in (or asked into) LAN mode, or `network` asked explicitly
/// (pairing mode, `kmplify-node router pair --lan`); else this computer.
pub fn scope_for(config: &super::Config, network: bool) -> Scope {
    if !config.allow.is_empty() && (network || config.asked == Mode::Lan) {
        Scope::Network
    } else {
        Scope::ThisComputer
    }
}

fn shown_addresses(scope: Scope, allow: &Allowlist, port: u16) -> Vec<String> {
    match scope {
        Scope::ThisComputer => vec![format!("127.0.0.1:{port}")],
        Scope::Network => if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .map(|i| policy::canonical(i.ip()))
            .filter(|ip| !ip.is_loopback() && allow.allows(*ip))
            .map(|ip| SocketAddr::new(ip, port).to_string())
            .collect(),
    }
}

impl Host {
    /// Open an invitation for the node in `node_dir`: load the cluster key
    /// from `keys` (or make and store one), pick a PIN, listen on `port`.
    pub fn open(
        node_dir: &std::path::Path,
        keys: &dyn KeyStore,
        network: bool,
        port: u16,
    ) -> Result<Host, String> {
        let config = config_for(node_dir);
        let key = match keys.load()? {
            Some(k) => k,
            None => {
                let k = ClusterKey::generate();
                keys.save(&k)?;
                k
            }
        };
        let scope = scope_for(&config, network);
        Self::open_with(key, config.name, config.allow, scope, port)
    }

    /// [`Host::open`] with everything given (the tests, and embedders).
    pub fn open_with(
        key: ClusterKey,
        name: String,
        allow: Allowlist,
        scope: Scope,
        port: u16,
    ) -> Result<Host, String> {
        let mode = match scope {
            Scope::ThisComputer => Mode::Local,
            Scope::Network => Mode::Lan,
        };
        let addr = policy::bind_addr(mode, port).ok_or("nothing to bind")?;
        let std_listener = std::net::TcpListener::bind(addr)
            .map_err(|e| format!("cannot listen for pairing on {addr}: {e}"))?;
        let bound = std_listener.local_addr().map_err(|e| e.to_string())?;
        policy::check_bound(mode, &bound)?;
        std_listener
            .set_nonblocking(true)
            .map_err(|e| e.to_string())?;
        let fingerprint = key.fingerprint();
        let inner = Arc::new(Mutex::new(HostInner {
            inviter: Some(Inviter::new(new_pin(), key, name, crate::status::now_ms())),
            scope,
            addresses: shown_addresses(scope, &allow, bound.port()),
            fingerprint,
            message: String::new(),
            failed: false,
        }));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task_inner = inner.clone();
        std::thread::Builder::new()
            .name("router-pairing".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    lock(&task_inner).failed = true;
                    return;
                };
                rt.block_on(serve(std_listener, task_inner, mode, allow, rx));
            })
            .map_err(|e| e.to_string())?;
        Ok(Host {
            inner,
            stop: Some(tx),
        })
    }

    /// The port it listens on (from the first address shown).
    pub fn view(&self) -> PairView {
        let now = crate::status::now_ms();
        let mut g = lock(&self.inner);
        let scope = match g.scope {
            Scope::ThisComputer => "this computer only".to_string(),
            Scope::Network => "the network, allowlist only".to_string(),
        };
        let addresses = g.addresses.clone();
        let fingerprint = g.fingerprint.clone();
        let message = g.message.clone();
        let failed = g.failed;
        let Some(inv) = g.inviter.as_mut() else {
            return PairView::default();
        };
        let mut state = inv.state(now);
        if failed && state == PairState::Waiting {
            state = PairState::Failed;
        }
        PairView {
            state,
            state_label: state.label().into(),
            pin: if state == PairState::Waiting {
                inv.pin().to_string()
            } else {
                String::new()
            },
            seconds_left: if state == PairState::Waiting {
                inv.expires_ms().saturating_sub(now).div_ceil(1000)
            } else {
                0
            },
            attempts_left: inv.attempts_left(),
            scope,
            addresses,
            fingerprint,
            joined: inv.joined().to_string(),
            message,
        }
    }

    pub fn is_open(&self) -> bool {
        self.view().state == PairState::Waiting
    }

    pub fn close(&mut self) {
        if let Some(inv) = lock(&self.inner).inviter.as_mut() {
            inv.close();
        }
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.close();
    }
}

async fn serve(
    std_listener: std::net::TcpListener,
    inner: Arc<Mutex<HostInner>>,
    mode: Mode,
    allow: Allowlist,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else {
        lock(&inner).failed = true;
        return;
    };
    let deadline = tokio::time::Instant::now() + PIN_TTL + Duration::from_secs(1);
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = tokio::time::sleep_until(deadline) => break,
            _ = &mut stop => break,
        };
        let Ok((mut stream, peer)) = accepted else {
            continue;
        };
        if policy::admit(mode, &allow, peer.ip()).is_err() {
            // Not loopback, or not on the allowlist: closed unread.
            continue;
        }
        // One request at a time: pairing is two short requests from one
        // device, and serial handling keeps the attempt count exact.
        let req = match tokio::time::timeout(http::READ_DEADLINE, http::read_request(&mut stream))
            .await
        {
            Ok(Ok(req)) => req,
            Ok(Err(code)) => {
                http::write_response(&mut stream, &Response::text(code, "bad request")).await;
                continue;
            }
            Err(_) => continue,
        };
        let resp = {
            let mut g = lock(&inner);
            let now = crate::status::now_ms();
            let resp = match g.inviter.as_mut() {
                Some(inv) => inv.answer(&req, now),
                None => Response::text(403, "no pairing is open"),
            };
            if let Some(inv) = g.inviter.as_mut() {
                let st = inv.state(now);
                g.message = match st {
                    PairState::Paired => "paired; this invitation is closed".into(),
                    PairState::Void => "three wrong PINs; this PIN is void".into(),
                    _ if resp.status == 403 => "a wrong PIN was entered".into(),
                    _ => std::mem::take(&mut g.message),
                };
            }
            resp
        };
        http::write_response(&mut stream, &resp).await;
        let done = lock(&inner)
            .inviter
            .as_mut()
            .map(|i| i.state(crate::status::now_ms()) != PairState::Waiting)
            .unwrap_or(true);
        if done {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000_000;

    fn inviter(pin: &str) -> (Inviter, ClusterKey) {
        let key = ClusterKey::generate();
        (
            Inviter::new(pin.into(), key.clone(), "studio".into(), T0),
            key,
        )
    }

    fn run(inv: &mut Inviter, pin: &str, now: u64) -> Result<Joined, String> {
        let s = JoinStart::new(pin, "laptop");
        let r = inv.start(&s.request, now).map_err(|(_, e)| e.error)?;
        let f = s.step(&r)?;
        let done = inv.finish(&f.request, now).map_err(|(_, e)| e.error)?;
        f.open(&done)
    }

    #[test]
    fn pins_are_six_digits_and_parse_leniently() {
        for _ in 0..200 {
            let p = new_pin();
            assert_eq!(p.len(), 6);
            assert!(p.bytes().all(|b| b.is_ascii_digit()));
        }
        assert_eq!(parse_pin(" 123-456 ").unwrap(), "123456");
        assert!(parse_pin("12345").is_err());
        assert!(parse_pin("12345a").is_err());
    }

    /// Round trip: the right PIN carries the key across, sealed, and both
    /// sides end with the same fingerprint.
    #[test]
    fn the_right_pin_hands_over_the_key() {
        let (mut inv, key) = inviter("482913");
        let joined = run(&mut inv, "482913", T0 + 1_000).unwrap();
        assert_eq!(joined.key, key);
        assert_eq!(joined.fingerprint, key.fingerprint());
        assert_eq!(joined.inviter_name, "studio");
        assert_eq!(inv.state(T0 + 1_000), PairState::Paired);
        assert_eq!(inv.joined(), "laptop");
        // One device per PIN.
        assert!(run(&mut inv, "482913", T0 + 2_000).is_err());
    }

    /// The key schedule and the seal, pinned: a known answer for the parts
    /// after SPAKE2 (whose own messages are random per run), so another
    /// implementation (or a refactor) can be checked against it.
    #[test]
    fn the_key_schedule_has_a_known_answer() {
        let th = transcript(&[1u8; 33], &[2u8; 33], "00112233445566778899aabbccddeeff");
        let keys = schedule(&[7u8; 32], &th);
        assert_eq!(
            hex(&th),
            hex(&Sha256::digest(
                [
                    TRANSCRIPT_LABEL,
                    &[0, 33],
                    &[1u8; 33],
                    &[0, 33],
                    &[2u8; 33],
                    &[0, 32],
                    b"00112233445566778899aabbccddeeff"
                ]
                .concat()
            ))
        );
        // Independent HKDF over the same inputs.
        let hk = hkdf::Hkdf::<Sha256>::new(Some(&th), &[7u8; 32]);
        let mut want = [0u8; 32];
        hk.expand(b"kmplify-router-v2 seal cluster key", &mut want)
            .unwrap();
        assert_eq!(keys.seal, want);
        assert_ne!(keys.joiner_confirm, keys.inviter_confirm);
        assert_ne!(keys.seal, keys.joiner_confirm);
    }

    /// SPAKE2 itself, with a fixed random source: the same password gives
    /// the same key on both sides, a different one does not.
    #[test]
    fn spake2_agrees_only_on_the_same_pin() {
        let pw = Password::new(b"482913");
        let (a, ma) = Spake2::<Ed25519Group>::start_a(
            &pw,
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        let (b, mb) = Spake2::<Ed25519Group>::start_b(
            &pw,
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        assert_eq!(ma.len(), 33);
        assert_eq!(a.finish(&mb).unwrap(), b.finish(&ma).unwrap());
        let (a, ma) = Spake2::<Ed25519Group>::start_a(
            &Password::new(b"000000"),
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        let (b, mb) = Spake2::<Ed25519Group>::start_b(
            &pw,
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        assert_ne!(a.finish(&mb).unwrap(), b.finish(&ma).unwrap());
    }

    #[test]
    fn a_wrong_pin_learns_nothing_and_costs_an_attempt() {
        let (mut inv, _) = inviter("482913");
        let s = JoinStart::new("111111", "laptop");
        let r = inv.start(&s.request, T0).unwrap();
        let f = s.step(&r).unwrap();
        let (code, why) = inv.finish(&f.request, T0).unwrap_err();
        assert_eq!(code, 403);
        assert_eq!(why.error, "wrong PIN");
        assert_eq!(why.attempts_left, 2);
        // Still open for the right one.
        assert!(run(&mut inv, "482913", T0).is_ok());
    }

    #[test]
    fn three_wrong_pins_void_the_invitation() {
        let (mut inv, _) = inviter("482913");
        for left in [2, 1] {
            let e = run(&mut inv, "000001", T0).unwrap_err();
            assert_eq!(e, "wrong PIN", "{left}");
        }
        assert_eq!(
            run(&mut inv, "000002", T0).unwrap_err(),
            "wrong PIN; this PIN is now void"
        );
        assert_eq!(inv.state(T0), PairState::Void);
        // The right PIN is refused from now on.
        assert!(run(&mut inv, "482913", T0).unwrap_err().contains("void"));
    }

    #[test]
    fn a_pin_expires_after_two_minutes() {
        let (mut inv, _) = inviter("482913");
        assert_eq!(inv.state(T0 + PIN_TTL_MS - 1), PairState::Waiting);
        let e = run(&mut inv, "482913", T0 + PIN_TTL_MS).unwrap_err();
        assert!(e.contains("expired"), "{e}");
        assert_eq!(inv.state(T0 + PIN_TTL_MS), PairState::Expired);
    }

    #[test]
    fn starting_sessions_without_finishing_tests_no_guess() {
        // Starts cost nothing and say nothing that depends on the PIN; only
        // a finish (a committed guess) is counted.
        let (mut inv, _) = inviter("482913");
        for _ in 0..20 {
            let s = JoinStart::new("999999", "x");
            let r = inv.start(&s.request, T0).unwrap();
            assert_eq!(r.msg.len(), 66);
        }
        assert_eq!(inv.attempts_left(), MAX_WRONG);
        assert!(run(&mut inv, "482913", T0).is_ok());
    }

    /// A man in the middle without the PIN: it relays the joiner's messages
    /// but has to run its own SPAKE2 toward each side with a guessed PIN.
    /// It learns nothing, gets no key, the joiner fails, and its guess at
    /// the inviter costs an attempt.
    #[test]
    fn a_man_in_the_middle_without_the_pin_gets_nothing() {
        let (mut inv, key) = inviter("482913");
        let guess = "123456";
        // The joiner talks to the attacker, who poses as the inviter.
        let joiner = JoinStart::new("482913", "laptop");
        let mut fake = Inviter::new(guess.into(), ClusterKey::generate(), "studio".into(), T0);
        let r = fake.start(&joiner.request, T0).unwrap();
        let jf = joiner.step(&r).unwrap();
        // The joiner's confirmation does not match the attacker's guess...
        assert!(fake.finish(&jf.request, T0).is_err());
        // ...and toward the real inviter the attacker must commit a guess.
        let toward = JoinStart::new(guess, "laptop");
        let r = inv.start(&toward.request, T0).unwrap();
        let tf = toward.step(&r).unwrap();
        assert_eq!(
            inv.finish(&tf.request, T0).unwrap_err().1.error,
            "wrong PIN"
        );
        assert_eq!(inv.attempts_left(), MAX_WRONG - 1);

        // Passive relay: the attacker forwards the real messages unchanged
        // and keeps the transcript. With any guessed PIN it cannot open the
        // sealed key, and a forged final answer does not fool the joiner.
        let joiner = JoinStart::new("482913", "laptop");
        let a = unhex(&joiner.request.msg).unwrap();
        let r = inv.start(&joiner.request, T0).unwrap();
        let b = unhex(&r.msg).unwrap();
        let jf = joiner.step(&r).unwrap();
        let done = inv.finish(&jf.request, T0).unwrap();
        let (eve, _) = Spake2::<Ed25519Group>::start_a(
            &Password::new(guess.as_bytes()),
            &Identity::new(ID_JOINER),
            &Identity::new(ID_INVITER),
        );
        let k_eve = eve.finish(&b).unwrap();
        let th = transcript(&a, &b, &r.session);
        let ks = schedule(&k_eve, &th);
        let cipher = Aes256Gcm::new_from_slice(&ks.seal).unwrap();
        assert!(cipher
            .decrypt(
                Nonce::from_slice(&unhex(&done.nonce).unwrap()),
                Payload {
                    msg: &unhex(&done.sealed).unwrap(),
                    aad: &aad(&th, &done.fingerprint),
                },
            )
            .is_err());
        // The genuine answer opens for the genuine joiner.
        assert_eq!(jf.open(&done).unwrap().key, key);
    }

    #[test]
    fn a_forged_final_answer_is_refused() {
        let (mut inv, _) = inviter("482913");
        let s = JoinStart::new("482913", "laptop");
        let r = inv.start(&s.request, T0).unwrap();
        let f = s.step(&r).unwrap();
        let mut done = inv.finish(&f.request, T0).unwrap();
        // Another key under the same fingerprint text: the seal's AAD binds
        // the fingerprint, and the opened key must match it.
        done.fingerprint = ClusterKey::generate().fingerprint();
        assert!(f.open(&done).is_err());
    }

    #[test]
    fn targets_are_addresses_on_loopback_or_the_allowlist() {
        let allow = Allowlist::parse(&["192.168.1.0/24"]).unwrap();
        assert_eq!(
            parse_target("127.0.0.1:5000", &Allowlist::default()).unwrap(),
            "127.0.0.1:5000".parse().unwrap()
        );
        assert_eq!(
            parse_target("192.168.1.9", &allow).unwrap().port(),
            pair_port()
        );
        assert!(parse_target("192.168.2.9", &allow).is_err());
        assert!(parse_target("8.8.8.8:1", &allow).is_err());
        assert!(parse_target("studio.local", &allow).is_err());
    }

    #[test]
    fn nothing_secret_in_debug_output() {
        let (inv, key) = inviter("482913");
        let text = format!("{inv:?}");
        assert!(!text.contains("482913"));
        assert!(!text.contains(&key.reveal()));
    }

    /// End to end over loopback: a host on 127.0.0.1 with a free port, a
    /// joiner, the key across; no mDNS anywhere, never the LAN.
    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_over_loopback_end_to_end() {
        let key = ClusterKey::generate();
        let mut host = Host::open_with(
            key.clone(),
            "studio".into(),
            Allowlist::default(),
            Scope::ThisComputer,
            0,
        )
        .unwrap();
        let view = host.view();
        assert_eq!(view.state, PairState::Waiting);
        assert_eq!(view.pin.len(), 6);
        let at: SocketAddr = view.addresses[0].parse().unwrap();
        assert!(at.ip().is_loopback());
        // A wrong PIN first.
        let e = join(at, "000000", "laptop").await;
        if view.pin != "000000" {
            assert!(e.unwrap_err().starts_with("wrong PIN (2"));
        }
        let joined = join(at, &view.pin, "laptop").await.unwrap();
        assert_eq!(joined.key, key);
        let after = host.view();
        assert_eq!(after.state, PairState::Paired);
        assert!(after.pin.is_empty(), "the PIN leaves the screen once used");
        assert_eq!(after.joined, "laptop");
        host.close();
    }

    /// Two node directories on this computer, as the CLI and the views use
    /// them: the first makes its key at `pair`, the second stores the same
    /// key in its own `router.key` after `join --pin`. Loopback only (no
    /// allowlist in either directory), no router running, no mDNS.
    #[test]
    fn two_node_dirs_pair_over_loopback() {
        use super::super::key::{FileKeyStore, KeyStore};
        let base = std::env::temp_dir().join(format!("kn-pair-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (ka, kb) = (FileKeyStore::new(&a), FileKeyStore::new(&b));
        assert_eq!(ka.load(), Ok(None));
        let mut host = Host::open(&a, &ka, false, 0).unwrap();
        let made = ka
            .load()
            .unwrap()
            .expect("pair makes a key when there is none");
        let v = host.view();
        assert_eq!(v.scope, "this computer only");
        assert_eq!(v.fingerprint, made.fingerprint());
        let at = v.addresses[0].clone();
        // A wrong PIN stores nothing.
        let wrong = if v.pin == "000000" {
            "000001"
        } else {
            "000000"
        };
        assert!(join_blocking(&b, &kb, &at, wrong).is_err());
        assert_eq!(kb.load(), Ok(None));
        let j = join_blocking(&b, &kb, &at, &v.pin).unwrap();
        assert_eq!(j.fingerprint, made.fingerprint());
        assert_eq!(kb.load(), Ok(Some(made)));
        assert_eq!(host.view().state, PairState::Paired);
        // The PIN is in no file of either directory.
        for d in [&a, &b] {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                assert!(!text.contains(&v.pin), "{}", e.path().display());
            }
        }
        host.close();
        let _ = std::fs::remove_dir_all(&base);
    }
}
