//! Mutual TLS between the members of one cluster (router protocol v2).
//!
//! Phase 1 sent reports as signed plain HTTP: authenticated, but readable by
//! anyone sniffing the LAN. From v2 a member asks another member over TLS 1.3
//! in which **both** sides present a certificate, and a certificate counts
//! only if the cluster key vouches for it:
//!
//! * every router start makes a fresh key pair and a self-signed
//!   certificate ([`BoundIdentity::mint`]); nothing of it is stored, so an
//!   embedding application (kmplify-desktop) keeps no second secret;
//! * the certificate carries, as a DNS name in its subject alternative
//!   names, the **binding tag** `HMAC-SHA256(cluster key, "kmplify-router-v2
//!   \ncert\n" || SHA-256(SubjectPublicKeyInfo))`, split in two 32-hex labels
//!   under `bind.kmplify-router.invalid`;
//! * the verifier on both ends ([`BoundVerifier`]) recomputes the tag over
//!   the public key actually in the certificate and accepts nothing else: no
//!   certificate authority, no host name, no clock. rustls then checks the
//!   handshake signature with that public key, so the peer also proves it
//!   holds the private key.
//!
//! A machine without the cluster key cannot make a tag for its own key, and
//! cannot reuse a member's certificate without that member's private key.
//! The tag is over the public key, not the certificate's hash (the hash
//! would include the tag), and is the "fingerprint bound to the cluster key"
//! of the port plan. A member's certificate fingerprint (SHA-256 of the DER)
//! is shown in the router state for comparison, see [`fingerprint`].

use std::sync::Arc;

use hmac::{Hmac, Mac};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};

use super::key::{hex, unhex, ClusterKey, DOMAIN};

type HmacSha256 = Hmac<Sha256>;

/// The DNS suffix of the binding name. `.invalid` (RFC 6761) never resolves.
pub const BIND_SUFFIX: &str = "bind.kmplify-router.invalid";
/// The first byte of a TLS handshake record: how the node-info surface tells
/// a v2 (TLS) caller from a v1 (signed plain HTTP) one on the same port.
pub const TLS_HANDSHAKE_BYTE: u8 = 0x16;

fn tag_mac(key: &ClusterKey, spki_der: &[u8]) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key.bytes()).expect("HMAC accepts any key length");
    m.update(format!("{DOMAIN}\nv2\ncert\n").as_bytes());
    m.update(&Sha256::digest(spki_der));
    m
}

fn binding_tag(key: &ClusterKey, spki_der: &[u8]) -> [u8; 32] {
    tag_mac(key, spki_der).finalize().into_bytes().into()
}

fn binding_name(tag: &[u8; 32]) -> String {
    let h = hex(tag);
    format!("{}.{}.{BIND_SUFFIX}", &h[..32], &h[32..])
}

/// SHA-256 over a certificate's DER, hex: what a screen shows to compare.
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex(&Sha256::digest(cert_der))
}

/// Does the cluster key vouch for this certificate's public key?
pub fn check_bound(key: &ClusterKey, cert_der: &[u8]) -> Result<(), String> {
    let der = CertificateDer::from(cert_der);
    let cert =
        webpki::EndEntityCert::try_from(&der).map_err(|e| format!("not a certificate: {e:?}"))?;
    let spki = cert.subject_public_key_info();
    let suffix = format!(".{BIND_SUFFIX}");
    for name in cert.valid_dns_names() {
        let Some(labels) = name.strip_suffix(&suffix) else {
            continue;
        };
        let Some(tag) = unhex(&labels.replace('.', "")) else {
            continue;
        };
        // Constant-time comparison against the tag over this certificate's
        // own public key.
        if tag_mac(key, spki.as_ref()).verify_slice(&tag).is_ok() {
            return Ok(());
        }
    }
    Err("the certificate is not bound to this cluster's key".into())
}

/// This router's certificate and private key for one cluster key. Made at
/// every start, kept in memory only.
pub struct BoundIdentity {
    pub cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: String,
}

impl Clone for BoundIdentity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key: self.key.clone_key(),
            fingerprint: self.fingerprint.clone(),
        }
    }
}

impl std::fmt::Debug for BoundIdentity {
    // Never the private key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BoundIdentity({})", &self.fingerprint[..16])
    }
}

impl BoundIdentity {
    /// A fresh ECDSA P-256 key pair and a self-signed certificate whose
    /// binding tag `key` made.
    pub fn mint(key: &ClusterKey) -> Result<Self, String> {
        use rcgen::PublicKeyData;
        let pair = rcgen::KeyPair::generate().map_err(|e| format!("key generation: {e}"))?;
        let tag = binding_tag(key, &pair.subject_public_key_info());
        let mut params = rcgen::CertificateParams::new(vec![binding_name(&tag)])
            .map_err(|e| format!("certificate parameters: {e}"))?;
        // A fixed, neutral subject: never the computer's or a person's name.
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "kmplify-router member");
        let cert = params
            .self_signed(&pair)
            .map_err(|e| format!("certificate: {e}"))?;
        let der = cert.der().clone();
        Ok(Self {
            fingerprint: fingerprint(der.as_ref()),
            cert: der,
            key: PrivatePkcs8KeyDer::from(pair.serialize_der()),
        })
    }

    fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts exactly the certificates the cluster key vouches for. The same
/// check on both ends: a member's view of a server and of a client.
#[derive(Debug)]
pub struct BoundVerifier {
    key: ClusterKey,
    provider: Arc<CryptoProvider>,
}

impl BoundVerifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        check_bound(&self.key, end_entity.as_ref()).map_err(rustls::Error::General)
    }

    fn tls12(
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

    fn tls13(
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
}

impl ServerCertVerifier for BoundVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ClientCertVerifier for BoundVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn verifier(key: &ClusterKey, provider: &Arc<CryptoProvider>) -> Arc<BoundVerifier> {
    Arc::new(BoundVerifier {
        key: key.clone(),
        provider: provider.clone(),
    })
}

/// The node-info surface's TLS: TLS 1.3 only, this router's certificate,
/// a client certificate demanded and checked against the cluster key.
pub fn server_config(id: &BoundIdentity, key: &ClusterKey) -> Result<Arc<ServerConfig>, String> {
    let provider = provider();
    ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(verifier(key, &provider))
        .with_single_cert(vec![id.cert.clone()], id.private_key())
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// A member asking another: TLS 1.3, the same check on the server's
/// certificate, this router's certificate as the client's.
pub fn client_config(id: &BoundIdentity, key: &ClusterKey) -> Result<ClientConfig, String> {
    let provider = provider();
    ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier(key, &provider))
        .with_client_auth_cert(vec![id.cert.clone()], id.private_key())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_certificate_is_bound_to_its_key_only() {
        let key = ClusterKey::generate();
        let id = BoundIdentity::mint(&key).unwrap();
        assert!(check_bound(&key, id.cert.as_ref()).is_ok());
        assert!(check_bound(&ClusterKey::generate(), id.cert.as_ref()).is_err());
        assert_eq!(id.fingerprint.len(), 64);
        assert!(!format!("{id:?}").contains("PRIVATE"));
    }

    #[test]
    fn the_tag_covers_the_public_key_actually_in_the_certificate() {
        // An attacker copies a member's binding name into a certificate for
        // a key of its own: the tag no longer matches that key.
        let key = ClusterKey::generate();
        let member = BoundIdentity::mint(&key).unwrap();
        let der = CertificateDer::from(member.cert.as_ref());
        let names: Vec<String> = webpki::EndEntityCert::try_from(&der)
            .unwrap()
            .valid_dns_names()
            .map(String::from)
            .collect();
        let pair = rcgen::KeyPair::generate().unwrap();
        let forged = rcgen::CertificateParams::new(names)
            .unwrap()
            .self_signed(&pair)
            .unwrap();
        assert!(check_bound(&key, forged.der().as_ref()).is_err());
        // And garbage is refused, not a panic.
        assert!(check_bound(&key, b"not der").is_err());
    }

    #[test]
    fn rcgen_and_webpki_agree_on_the_public_key_bytes() {
        use rcgen::PublicKeyData;
        let pair = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["a.invalid".into()])
            .unwrap()
            .self_signed(&pair)
            .unwrap();
        let der = cert.der().clone();
        let parsed = webpki::EndEntityCert::try_from(&der).unwrap();
        assert_eq!(
            parsed.subject_public_key_info().as_ref(),
            pair.subject_public_key_info().as_slice()
        );
    }
}
