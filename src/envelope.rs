//! Envelope encryption: a consumer seals its request to THIS node, and the
//! relay in between carries bytes it cannot read (protocol v5).
//!
//! The gateway used to see every prompt and every answer in the clear. That
//! makes its operator a processor of all of it, and a compromise of the
//! gateway a compromise of everything in flight. With the envelope the
//! gateway still routes, schedules and meters, on metadata alone.
//!
//! What this is NOT: the node decrypts. Whoever operates this machine can
//! read what it processes. This is not confidential computing and not zero
//! knowledge, and nothing may say so. The honest sentence is: *the relay
//! cannot read it.*
//!
//! The construction is HPKE (RFC 9180), base mode, ONE suite:
//! `DHKEM(X25519, HKDF-SHA256)`, `HKDF-SHA256`, `AES-256-GCM`. One suite and no
//! negotiation on purpose: a suite list is an attack surface, and the fabric
//! controls both ends. AES-256-GCM rather than the ChaCha20-Poly1305 the design
//! document first named, because WebCrypto has AES-GCM and X25519 natively and
//! has no ChaCha20: choosing it keeps the browser client free of WASM crypto
//! AND keeps the suite table at one row. The suite id travels on the wire so
//! this can change with a protocol version.
//!
//! - The node holds an X25519 key that is NOT its identity key. Signing and
//!   key agreement under one key is how a flaw in one becomes a break of the
//!   other, and KIP-1 promises the identity key only ever signs statements.
//! - The identity key ATTESTS the encryption key (KIP-1 purpose
//!   `node-encryption-key`). The consumer verifies that attestation itself,
//!   against a node identity it pinned. A consumer that takes the encryption
//!   key on the gateway's word has encrypted to whoever the gateway says.
//! - `info` binds a context to this node's identity, this key and the lane, so
//!   a context opened for inference cannot be replayed anywhere else. `aad`
//!   is the request's clear routing metadata, so the gateway cannot move a
//!   ciphertext under a different model.
//! - Answers go back under a key both sides derive from the context
//!   (`Export`), AES-256-GCM with a counter nonce, one per chunk. Streaming
//!   needs no second handshake, and a dropped, reordered or re-labelled chunk
//!   fails authentication instead of corrupting silently.
//!
//! No forward secrecy against a compromise of this key: HPKE base mode has
//! none. Whoever gets the secret reads every recorded conversation sealed to
//! it. Rotation (delete the key file, the node makes a new one and attests
//! it) bounds that window, and saying so is part of the design.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hpke::aead::AesGcm256;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem as KemTrait, OpModeR, Serializable};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

type Kem = X25519HkdfSha256;

/// The one suite. See the module comment for why there is only one.
pub const SUITE: u64 = 1;
/// KIP-1 purpose under which the identity key attests the encryption key.
pub const PURPOSE_NODE_ENCRYPTION_KEY: &str = "node-encryption-key";
/// Lane label bound into `info` for chat and embeddings jobs.
pub const LANE_INFERENCE: &str = "inference";

const INFO_PREFIX: &str = "KMPLIFY-ENV-v1";
const RESPONSE_EXPORT_LABEL: &[u8] = b"KMPLIFY-ENV-v1 response";

/// This node's encryption key pair, derived from a 32 byte seed.
pub struct EnvelopeKey {
    seed: [u8; 32],
    secret: <Kem as KemTrait>::PrivateKey,
    public: <Kem as KemTrait>::PublicKey,
}

impl EnvelopeKey {
    /// Deterministic from the seed (RFC 9180 `DeriveKeyPair`), so the file on
    /// disk holds 32 bytes and nothing format-specific.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let (secret, public) = Kem::derive_keypair(&seed);
        Self {
            seed,
            secret,
            public,
        }
    }

    pub fn from_seed_hex(seed_hex: &str) -> Option<Self> {
        let seed: [u8; 32] = crate::functions::hex_decode(seed_hex.trim())?
            .try_into()
            .ok()?;
        Some(Self::from_seed(seed))
    }

    pub fn generate() -> Self {
        use rand_core::RngCore;
        let mut seed = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut seed);
        Self::from_seed(seed)
    }

    pub fn seed_hex(&self) -> String {
        crate::identity::hex_encode(&self.seed)
    }

    pub fn public_hex(&self) -> String {
        crate::identity::hex_encode(&self.public.to_bytes())
    }

    /// First 8 bytes of SHA-256 of the public key, hex. Lets a consumer and
    /// the gateway say WHICH key a request was sealed to, so a rotation
    /// produces a clear "key changed" instead of an opaque decrypt failure.
    pub fn key_id(&self) -> String {
        crate::identity::hex_encode(&Sha256::digest(self.public.to_bytes())[..8])
    }

    /// The `envelope` block of a hello: the public key and the identity
    /// key's signature over it.
    pub fn attestation(&self, identity: &crate::identity::NodeKey, ts: u64) -> Value {
        let enc_pubkey = self.public_hex();
        let key_id = self.key_id();
        let sig = identity.sign(
            PURPOSE_NODE_ENCRYPTION_KEY,
            &canonical_attestation(&enc_pubkey, &key_id, &identity.public_hex(), ts),
        );
        json!({
            "suite": SUITE, "enc_pubkey": enc_pubkey, "key_id": key_id,
            "pubkey": identity.public_hex(), "ts": ts, "sig": sig,
        })
    }

    /// Open a sealed request and get the means to seal its answer.
    ///
    /// Every failure is the same error on purpose: telling a caller WHY a
    /// ciphertext did not open (wrong key, wrong aad, wrong lane) is an
    /// oracle, and none of them is something the caller can fix by retrying.
    pub fn open(
        &self,
        node_pubkey_hex: &str,
        lane: &str,
        enc: &[u8],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<(Vec<u8>, ResponseSealer), OpenError> {
        let encapped = <Kem as KemTrait>::EncappedKey::from_bytes(enc).map_err(|_| OpenError)?;
        let mut ctx = hpke::setup_receiver::<AesGcm256, HkdfSha256, Kem>(
            &OpModeR::Base,
            &self.secret,
            &encapped,
            &info(node_pubkey_hex, &self.key_id(), lane),
        )
        .map_err(|_| OpenError)?;
        let plaintext = ctx.open(ciphertext, aad).map_err(|_| OpenError)?;
        let mut key = [0u8; 32];
        ctx.export(RESPONSE_EXPORT_LABEL, &mut key)
            .map_err(|_| OpenError)?;
        Ok((plaintext, ResponseSealer { key, next: 0 }))
    }
}

/// A sealed request did not open. Deliberately carries no reason.
#[derive(Debug, PartialEq, Eq)]
pub struct OpenError;

/// What binds an HPKE context to one node, one key and one lane.
pub fn info(node_pubkey_hex: &str, key_id: &str, lane: &str) -> Vec<u8> {
    format!("{INFO_PREFIX}|{node_pubkey_hex}|{key_id}|{lane}").into_bytes()
}

/// Canonical JSON of the attestation payload: keys sorted, no whitespace.
/// No node id in it: consumers only ever see a node's id PREFIX, and an
/// attestation they cannot rebuild is one they cannot verify.
pub fn canonical_attestation(enc_pubkey: &str, key_id: &str, pubkey: &str, ts: u64) -> Vec<u8> {
    format!(
        "{{\"enc_pubkey\":{},\"key_id\":{},\"pubkey\":{},\"suite\":{},\"ts\":{}}}",
        json!(enc_pubkey),
        json!(key_id),
        json!(pubkey),
        SUITE,
        ts
    )
    .into_bytes()
}

/// The clear routing metadata of an inference request, as `aad`. The node
/// rebuilds it from what the GATEWAY put in the job, so if the gateway
/// changed the model or the streaming flag, the ciphertext does not open.
pub fn inference_aad(kind: &str, model: &str, stream: bool, ts: u64) -> Vec<u8> {
    format!(
        "{{\"kind\":{},\"model\":{},\"stream\":{},\"ts\":{}}}",
        json!(kind),
        json!(model),
        stream,
        ts
    )
    .into_bytes()
}

/// How far a sealed request's `ts` may be from this node's clock.
pub const FRESHNESS_S: u64 = 300;

/// Refuses a sealed request this node has already opened.
///
/// This is not tidiness, it is what keeps AES-GCM safe here. The answer key
/// and its counter nonces are derived from the REQUEST. A relay that replays
/// a captured request makes the node run the job again, and a language model
/// does not give the same answer twice, so two different plaintexts would be
/// sealed under the same key and the same nonces. With GCM that leaks the XOR
/// of the answers and the authentication key. The relay is exactly the party
/// this design does not trust, so a replay has to fail BEFORE a job runs.
///
/// The consumer puts a timestamp in the request and authenticates it (it is
/// part of `aad`). A request outside the freshness window is refused, which
/// bounds how long an `enc` value has to be remembered; one from before this
/// process started is refused too, because the memory that would have caught
/// its replay did not survive the restart.
pub struct ReplayGuard {
    started: u64,
    seen: std::collections::HashMap<Vec<u8>, u64>,
}

impl ReplayGuard {
    pub fn new(now: u64) -> Self {
        Self {
            started: now,
            seen: std::collections::HashMap::new(),
        }
    }

    /// True exactly once per `enc`, and only for a fresh timestamp.
    pub fn admit(&mut self, enc: &[u8], ts: u64, now: u64) -> bool {
        if ts < self.started || ts + FRESHNESS_S < now || ts > now + FRESHNESS_S {
            return false;
        }
        // Anything older than the window can no longer be admitted at all, so
        // it no longer has to be remembered.
        self.seen
            .retain(|_, seen_ts| *seen_ts + 2 * FRESHNESS_S >= now);
        self.seen.insert(enc.to_vec(), ts).is_none()
    }
}

/// Seals the answer to one opened request, chunk by chunk.
pub struct ResponseSealer {
    key: [u8; 32],
    next: u64,
}

impl ResponseSealer {
    /// `{"n", "k", "ct"}`: the chunk's index, its kind ("chunk", "done",
    /// "error") and the ciphertext, base64. Index and kind are authenticated,
    /// so a relay can neither reorder chunks nor pass one off as the end.
    pub fn seal(&mut self, kind: &str, plaintext: &[u8]) -> Value {
        let n = self.next;
        self.next += 1;
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("32 byte key");
        let ct = cipher
            .encrypt(
                &response_nonce(n),
                Payload {
                    msg: plaintext,
                    aad: response_aad(kind, n).as_bytes(),
                },
            )
            .expect("AES-GCM encryption does not fail for in-memory buffers");
        json!({"n": n, "k": kind, "ct": b64(&ct)})
    }
}

/// 96 bit nonce: the chunk counter, big endian, in the low 8 bytes. One key
/// per request and a counter that only goes up means a nonce never repeats.
fn response_nonce(n: u64) -> Nonce<aes_gcm::aes::cipher::consts::U12> {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&n.to_be_bytes());
    nonce.into()
}

fn response_aad(kind: &str, n: u64) -> String {
    format!("{kind}|{n}")
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const NODE_PUBKEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn key() -> EnvelopeKey {
        EnvelopeKey::from_seed_hex(SEED).unwrap()
    }

    /// The HPKE crate wants rand_core 0.9, the rest of this crate has 0.6.
    /// Only the SENDER needs randomness, and this node never sends, so the
    /// bridge lives in the tests and the binary carries no second RNG.
    struct TestRng;
    impl hpke::rand_core::RngCore for TestRng {
        fn next_u32(&mut self) -> u32 {
            rand_core::RngCore::next_u32(&mut rand_core::OsRng)
        }
        fn next_u64(&mut self) -> u64 {
            rand_core::RngCore::next_u64(&mut rand_core::OsRng)
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, dst)
        }
    }
    impl hpke::rand_core::CryptoRng for TestRng {}

    /// Seal the way a consumer does, with the same crate, for round trips.
    fn seal_for(
        k: &EnvelopeKey,
        lane: &str,
        aad: &[u8],
        msg: &[u8],
    ) -> (Vec<u8>, Vec<u8>, [u8; 32]) {
        use hpke::OpModeS;
        let mut rng = TestRng;
        let (enc, mut ctx) = hpke::setup_sender::<AesGcm256, HkdfSha256, Kem, _>(
            &OpModeS::Base,
            &k.public,
            &info(NODE_PUBKEY, &k.key_id(), lane),
            &mut rng,
        )
        .unwrap();
        let ct = ctx.seal(msg, aad).unwrap();
        let mut rk = [0u8; 32];
        ctx.export(RESPONSE_EXPORT_LABEL, &mut rk).unwrap();
        (enc.to_bytes().to_vec(), ct, rk)
    }

    #[test]
    fn a_sealed_request_opens_and_the_answer_key_matches_the_consumer_s() {
        let k = key();
        let aad = inference_aad("chat", "llama3.1:8b", true, 1_800_000_000);
        let (enc, ct, consumer_key) = seal_for(&k, LANE_INFERENCE, &aad, b"{\"messages\":[]}");
        let (pt, mut sealer) = k
            .open(NODE_PUBKEY, LANE_INFERENCE, &enc, &ct, &aad)
            .unwrap();
        assert_eq!(pt, b"{\"messages\":[]}");
        assert_eq!(sealer.key, consumer_key);
        // The consumer opens the answer with the exported key.
        let sealed = sealer.seal("chunk", b"hello");
        let cipher = Aes256Gcm::new_from_slice(&consumer_key).unwrap();
        let back = cipher
            .decrypt(
                &response_nonce(0),
                Payload {
                    msg: &unb64(sealed["ct"].as_str().unwrap()).unwrap(),
                    aad: b"chunk|0",
                },
            )
            .unwrap();
        assert_eq!(back, b"hello");
        assert_eq!(sealer.seal("done", b"x")["n"], 1);
    }

    /// The gateway cannot move a ciphertext under a different model or flip
    /// streaming, and a context for one lane or node opens nowhere else.
    #[test]
    fn anything_the_relay_could_change_stops_it_opening() {
        let k = key();
        let aad = inference_aad("chat", "llama3.1:8b", true, 1_800_000_000);
        let (enc, ct, _) = seal_for(&k, LANE_INFERENCE, &aad, b"secret");
        let open = |node: &str, lane: &str, aad: &[u8], ct: &[u8]| {
            k.open(node, lane, &enc, ct, aad).map(|r| r.0)
        };
        assert!(open(NODE_PUBKEY, LANE_INFERENCE, &aad, &ct).is_ok());
        assert_eq!(
            open(
                NODE_PUBKEY,
                LANE_INFERENCE,
                &inference_aad("chat", "gpt-oss:120b", true, 1_800_000_000),
                &ct
            ),
            Err(OpenError)
        );
        assert_eq!(
            open(
                NODE_PUBKEY,
                LANE_INFERENCE,
                &inference_aad("chat", "llama3.1:8b", false, 1_800_000_000),
                &ct
            ),
            Err(OpenError)
        );
        assert_eq!(open(NODE_PUBKEY, "relay", &aad, &ct), Err(OpenError));
        assert_eq!(
            open(&"ab".repeat(32), LANE_INFERENCE, &aad, &ct),
            Err(OpenError)
        );
        let mut flipped = ct.clone();
        flipped[0] ^= 1;
        assert_eq!(
            open(NODE_PUBKEY, LANE_INFERENCE, &aad, &flipped),
            Err(OpenError)
        );
        // Another node's key: nothing.
        let other = EnvelopeKey::from_seed([9u8; 32]);
        assert!(other
            .open(NODE_PUBKEY, LANE_INFERENCE, &enc, &ct, &aad)
            .is_err());
        assert!(k
            .open(NODE_PUBKEY, LANE_INFERENCE, b"short", &ct, &aad)
            .is_err());
    }

    /// The property that keeps GCM safe: a request opens ONCE.
    #[test]
    fn a_replayed_or_stale_request_is_never_admitted() {
        let now = 1_800_000_000;
        let mut guard = ReplayGuard::new(now - 1000);
        assert!(guard.admit(b"enc-1", now, now));
        assert!(!guard.admit(b"enc-1", now, now), "replay");
        assert!(!guard.admit(b"enc-1", now, now + 10), "replay, later");
        assert!(guard.admit(b"enc-2", now, now));
        // Outside the window in either direction.
        assert!(!guard.admit(b"enc-3", now - FRESHNESS_S - 1, now));
        assert!(!guard.admit(b"enc-4", now + FRESHNESS_S + 1, now));
        // From before this process started: its replay memory is gone.
        let mut restarted = ReplayGuard::new(now);
        assert!(!restarted.admit(b"enc-5", now - 1, now));
        assert!(restarted.admit(b"enc-5", now, now));
        // Forgotten only once it could no longer be admitted anyway.
        let later = now + 2 * FRESHNESS_S + 1;
        assert!(guard.admit(b"enc-9", later, later));
        assert!(!guard.seen.contains_key(b"enc-1".as_slice()));
        assert!(!guard.admit(b"enc-1", now, later), "too old to come back");
    }

    /// A chunk passed off as the end, or chunks swapped, fail authentication.
    #[test]
    fn answer_chunks_cannot_be_reordered_or_relabelled() {
        let mut sealer = ResponseSealer {
            key: [7u8; 32],
            next: 0,
        };
        let first = sealer.seal("chunk", b"one");
        let cipher = Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap();
        let ct = unb64(first["ct"].as_str().unwrap()).unwrap();
        let try_open = |n: u64, aad: &str| {
            cipher
                .decrypt(
                    &response_nonce(n),
                    Payload {
                        msg: &ct,
                        aad: aad.as_bytes(),
                    },
                )
                .is_ok()
        };
        assert!(try_open(0, "chunk|0"));
        assert!(!try_open(0, "done|0"));
        assert!(!try_open(1, "chunk|1"));
    }

    #[test]
    fn the_attestation_verifies_under_the_identity_key_and_no_other_purpose() {
        let identity = crate::identity::NodeKey::generate();
        let k = key();
        let att = k.attestation(&identity, 1_800_000_000);
        assert_eq!(att["suite"], 1);
        assert_eq!(att["key_id"].as_str().unwrap().len(), 16);
        let canonical = canonical_attestation(
            &k.public_hex(),
            &k.key_id(),
            &identity.public_hex(),
            1_800_000_000,
        );
        let sig = att["sig"].as_str().unwrap();
        assert!(crate::identity::verify(
            &identity.public_hex(),
            PURPOSE_NODE_ENCRYPTION_KEY,
            &canonical,
            sig
        ));
        assert!(!crate::identity::verify(
            &identity.public_hex(),
            crate::identity::PURPOSE_NODE_HELLO,
            &canonical,
            sig
        ));
    }

    /// Pinned against the gateway's Python reference client
    /// (tests/test_envelope.py pins the same values): same seed, same public
    /// key, same key id, same canonical bytes.
    #[test]
    fn the_key_derivation_and_canonical_forms_match_the_python_client() {
        let k = key();
        assert_eq!(k.seed_hex(), SEED);
        assert_eq!(
            String::from_utf8(canonical_attestation("aa", "bb", "cc", 5)).unwrap(),
            r#"{"enc_pubkey":"aa","key_id":"bb","pubkey":"cc","suite":1,"ts":5}"#
        );
        assert_eq!(
            String::from_utf8(inference_aad("chat", "m", true, 7)).unwrap(),
            r#"{"kind":"chat","model":"m","stream":true,"ts":7}"#
        );
        assert_eq!(
            String::from_utf8(info("pk", "kid", "inference")).unwrap(),
            "KMPLIFY-ENV-v1|pk|kid|inference"
        );
    }

    /// A request sealed by the gateway repo's PYTHON reference client
    /// (sdk/python/kmplify_envelope.py, fixed ephemeral key), pasted verbatim.
    /// Two independent HPKE implementations, one hand-written against the
    /// CFRG vector and one a crate, have to agree on every byte for this to
    /// open, and the Python side's test opens the answer sealed here.
    #[test]
    fn a_request_sealed_by_the_python_client_opens_here() {
        let k = key();
        assert_eq!(
            k.public_hex(),
            "b1f1b840de7a3241b02748cf9b05b74dc8c5e8451298738817bd76aa8ebe8c2b"
        );
        assert_eq!(k.key_id(), "01520b9fd72dc69a");
        let enc = crate::functions::hex_decode(
            "37fda3567bdbd628e88668c3c8d7e97d1d1253b6d4ea6d44c150f741f1bf4431",
        )
        .unwrap();
        let ct = crate::functions::hex_decode("00e4c3a6a39797dfae9350fa0c89888de245e831a4d9e5f127702ae711af08b0588522efb7ba03cdae8e7c530056cc536e308460447fae3349eaa50dc27493b1d2b339036afa5169a35b401e29f43840d592f9b183e0ae978eec0d33616dbf68bebceb74c1563f0811").unwrap();
        let aad = inference_aad("chat", "llama3.1:8b", true, 1_800_000_000);
        let (pt, mut sealer) = k
            .open(NODE_PUBKEY, LANE_INFERENCE, &enc, &ct, &aad)
            .unwrap();
        assert_eq!(
            String::from_utf8(pt).unwrap(),
            r#"{"messages":[{"content":"hello node","role":"user"}],"model":"llama3.1:8b","stream":true}"#
        );
        assert_eq!(
            crate::identity::hex_encode(&sealer.key),
            "e2ae444d83320cf4110265ac4366f139d37e1b5dc9927499a092d6f5ccd1dc84"
        );
        // Deterministic (counter nonce, exported key), so it can be pinned in
        // the Python test as the answer it must be able to open.
        let sealed = sealer.seal("chunk", b"hello consumer");
        assert_eq!(
            sealed,
            json!({"n": 0, "k": "chunk", "ct": "KDgl9jZ4HTDq+h0Z1UCjJHbTZykc/sJ3/CXjgw6h"})
        );
    }
}
