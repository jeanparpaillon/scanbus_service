//! The host's TLS identity: one certificate, presented in two roles.
//!
//! [`HostIdentity`] is the whole of mobile-backend.md §11.2 on this side of the wire. The
//! pairing connection the host dials presents this certificate as a *client* certificate
//! (§11.3); the upload listener presents the **same** one as its server certificate
//! (§11.4); and what the app pins is the SHA-256 of its DER. A second certificate minted
//! for the server role would compile, would pass every Rust-to-Rust test, and would be
//! refused by every phone that ever paired over TLS. So there is one [`CertifiedKey`]
//! here, built once at construction, and both configurations get a clone of that `Arc`.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, date_time_ymd,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};

/// The certificate, next to the device table of §8. It is public by nature — it is the
/// thing the phone pins — and PEM so that an operator can read it back with
/// `openssl x509 -in cert.pem -noout -text`.
pub const CERT_FILE: &str = "cert.pem";

/// The private key, mode `0600`, and the one file in §11.5 that is never regenerated.
pub const KEY_FILE: &str = "key.pem";

/// Nothing verifies this name, on either side. It is here because a certificate with an
/// empty subject is a thing some parsers dislike, and it is `scanbus` because that is
/// what the phone's own certificate carries (§11.3) — an operator looking at either end
/// of a pairing sees the same string and is not invited to read anything into it.
const COMMON_NAME: &str = "scanbus";

/// Both ends of the validity window, stated rather than inherited from
/// [`CertificateParams::default`], which happens to use the same two dates today.
///
/// §11.5: the certificate asserts nothing anybody validates — the app ignores dates,
/// chains and names on purpose — so the window is a formality, and the only thing an
/// expiry could achieve here is to invalidate every pairing on the host in order to
/// refresh a claim nothing reads. The lower bound is far enough back to survive a phone
/// whose clock has reset to its build date.
const NOT_BEFORE_YMD: (i32, u8, u8) = (1975, 1, 1);
const NOT_AFTER_YMD: (i32, u8, u8) = (4096, 1, 1);

/// The host's one TLS identity: a self-signed ECDSA P-256 certificate and its key.
///
/// `ECDSA P-256` with `SHA-256` because that is what the phone's own key is and what
/// every Android JSSE can verify; Ed25519 is not universally available on the Android
/// versions this protocol supports (§11.2).
pub struct HostIdentity {
    certified_key: Arc<CertifiedKey>,
    fingerprint: String,
    pairing_client_config: Arc<ClientConfig>,
    upload_server_config: Arc<ServerConfig>,
}

/// Redacted, like `PairedDevice`: the certificate bytes are noise
/// in a log line and the private key must not reach one through a `?identity` that
/// looked harmless at the call site. The fingerprint is the only part worth printing —
/// it is exactly what the phone pinned.
impl fmt::Debug for HostIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl HostIdentity {
    /// Loads the certificate and key from `dir`, generating them if this is a first
    /// start.
    ///
    /// `dir` is `$XDG_DATA_HOME/scanbus/mobile/` in a running daemon — the directory the
    /// device table already lives in (§11.5).
    ///
    /// Every failure below is returned rather than repaired. §11.5 is explicit that
    /// regenerating over a key that cannot be read is the one mistake in this design
    /// that cannot be walked back: it breaks every paired phone at once, silently, and
    /// each one surfaces later as a mysterious refusal on somebody's next scan. The
    /// caller's job is to log this loudly and bring the mobile scanners up `offline`.
    pub fn load_or_generate(dir: &Path) -> Result<Self, IdentityError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);

        match (cert_path.exists(), key_path.exists()) {
            (true, true) => Self::load(&cert_path, &key_path),
            (false, false) => Self::generate(dir, &cert_path, &key_path),
            // Half a pair is not a first start, and it is not a state this code can
            // reach on its own except by dying between the two file creations below. A
            // certificate without its key cannot be presented; a key without its
            // certificate cannot be turned back into the certificate that was pinned,
            // because the ECDSA signature over it is not deterministic. So there is
            // nothing to salvage either way — but treating it as a first start would
            // make "somebody moved the key aside to back it up" indistinguishable from
            // it, and that one *is* recoverable until we overwrite it.
            (cert_exists, _) => Err(IdentityError::Incomplete {
                present: if cert_exists {
                    cert_path.clone()
                } else {
                    key_path.clone()
                },
                missing: if cert_exists { key_path } else { cert_path },
            }),
        }
    }

    /// The certificate chain and signing key, for a `ClientConfig` or a `ServerConfig`.
    ///
    /// One `Arc`, cloned: see the module comment on why these must not be two.
    pub fn certified_key(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.certified_key)
    }

    /// The leaf certificate, DER, as the phone sees it in either handshake.
    pub fn certificate(&self) -> &CertificateDer<'static> {
        self.certified_key
            .cert
            .first()
            .expect("host identity is built from exactly one certificate")
    }

    /// Lowercase hex SHA-256 of [`Self::certificate`] — the value the app pins during
    /// pairing (§11.1), and the one identifying thing about this key pair that is safe
    /// to log.
    pub fn fingerprint_sha256(&self) -> &str {
        &self.fingerprint
    }

    /// The configuration for the one connection this host dials: the pairing connection
    /// of §11.3, where the host is the TLS **client** and this certificate is the client
    /// certificate the phone pins.
    ///
    /// Built once, here, and shared by every pairing — the config is immutable and
    /// `rustls` takes it by `Arc` anyway, so a per-pairing build would only be a chance
    /// for the two to drift.
    pub fn pairing_client_config(&self) -> Arc<ClientConfig> {
        Arc::clone(&self.pairing_client_config)
    }

    /// The configuration for the connections the *phone* dials: the upload listener of
    /// §11.4, where the host is the TLS **server** and presents this same certificate.
    ///
    /// "Same" is the requirement of §11.2 and it is met here by construction rather than
    /// by convention — both configurations are built from the one [`CertifiedKey`] above,
    /// in [`Self::assemble`], so there is no arrangement of this crate in which the
    /// listener can come to present a certificate other than the one a pairing pinned.
    pub fn upload_server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.upload_server_config)
    }

    fn load(cert_path: &Path, key_path: &Path) -> Result<Self, IdentityError> {
        let cert_pem = fs::read(cert_path).map_err(|source| IdentityError::Read {
            path: cert_path.to_path_buf(),
            source,
        })?;
        let key_pem = fs::read(key_path).map_err(|source| IdentityError::Read {
            path: key_path.to_path_buf(),
            source,
        })?;

        let certificate =
            CertificateDer::from_pem_slice(&cert_pem).map_err(|source| IdentityError::Parse {
                path: cert_path.to_path_buf(),
                source,
            })?;
        let key =
            PrivateKeyDer::from_pem_slice(&key_pem).map_err(|source| IdentityError::Parse {
                path: key_path.to_path_buf(),
                source,
            })?;

        Self::assemble(certificate, key)
    }

    fn generate(dir: &Path, cert_path: &Path, key_path: &Path) -> Result<Self, IdentityError> {
        crate::ensure_store_dir(dir).map_err(IdentityError::Directory)?;

        let key_pair =
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(IdentityError::Generate)?;

        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, COMMON_NAME);
        params.not_before = date_time_ymd(NOT_BEFORE_YMD.0, NOT_BEFORE_YMD.1, NOT_BEFORE_YMD.2);
        params.not_after = date_time_ymd(NOT_AFTER_YMD.0, NOT_AFTER_YMD.1, NOT_AFTER_YMD.2);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        // Both, spelled out, because this certificate really is used both ways (§11.2).
        // Absent EKUs would mean "any purpose" and work just as well against the peers
        // this protocol has, but a JSSE path that does check the extension would then
        // refuse one of the two roles, and the failure would arrive months later as an
        // unexplained handshake error on one side only.
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let certificate = params
            .self_signed(&key_pair)
            .map_err(IdentityError::Generate)?;

        // The key first, and `create_new` rather than truncate: this function is only
        // ever reached when neither file existed, so a failure here means something else
        // is writing this directory — a second daemon starting at the same moment — and
        // losing that race must not clobber the identity that other process is about to
        // use. The mode goes on at `open`, not with a `chmod` afterwards, so the key is
        // never on disk group-readable, not even for the width of one syscall.
        write_new(key_path, key_pair.serialize_pem().as_bytes(), 0o600)?;
        write_new(cert_path, certificate.pem().as_bytes(), 0o644)?;
        sync_dir(dir)?;

        Self::assemble(
            certificate.der().clone(),
            PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
        )
    }

    fn assemble(
        certificate: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, IdentityError> {
        let fingerprint = crate::hex_lower(&Sha256::digest(certificate.as_ref()));

        // `from_der` is what turns an unusable identity into a startup failure instead of
        // a handshake failure on the day somebody tries to scan: it parses the key with
        // the same provider that will later sign with it, and compares its public half
        // against the certificate's. A key and a certificate that do not belong to each
        // other is the shape a botched hand-rotation takes.
        //
        // The provider is named rather than taken from the process default: which one is
        // installed depends on load order and on which crate got there first, and this
        // identity has to be the one `ring` can use, since that is the provider both
        // configurations are built with (§11 dependency notes in the workspace manifest).
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certified_key = Arc::new(
            CertifiedKey::from_der(vec![certificate], key, &provider)
                .map_err(IdentityError::Unusable)?,
        );

        // Built here rather than at the first pairing so that a configuration `rustls`
        // will not accept is a startup failure with the rest of §11.5, and not a pairing
        // that fails on somebody's phone months later.
        let pairing_client_config =
            build_pairing_client_config(&provider, Arc::clone(&certified_key))
                .map_err(IdentityError::Unusable)?;
        let upload_server_config =
            build_upload_server_config(&provider, Arc::clone(&certified_key))
                .map_err(IdentityError::Unusable)?;

        Ok(Self {
            certified_key,
            fingerprint,
            pairing_client_config: Arc::new(pairing_client_config),
            upload_server_config: Arc::new(upload_server_config),
        })
    }
}

/// The `ClientConfig` of §11.3, and the only one in this crate.
///
/// Two things in it would be defects anywhere else, so they are stated here rather than
/// left to be discovered in review:
///
/// - **The phone's certificate is not verified.** [`AcceptAnyPhone`] returns success for
///   any chain. That is not the hole it looks like: the certificate is a self-signed
///   keystore certificate with `CN=scanbus`, from a phone this host has never spoken to,
///   reached at an IP address no name could match — there is nothing on this host that
///   could vouch for it, and no answer it could give that would mean anything. The phone
///   is not the identity being established here; *we* are, by the client certificate
///   below. What confirms the pairing in both directions is the six digits.
/// - **The client certificate is sent unconditionally.** [`SingleCertAndKey`] ignores the
///   `certificate_authorities` hint, and the phone's `CertificateRequest` carries none
///   because it accepts any issuer. A resolver that only answered for a recognised CA
///   would send nothing, the phone would refuse with "the computer sent no certificate to
///   pin", and the failure would read as a phone bug.
///
/// The verifier is private to this module and is constructed nowhere else, which is what
/// keeps it off the upload listener's configuration (§11.4) — there, the host is the
/// server and verifies nothing at all.
fn build_pairing_client_config(
    provider: &Arc<CryptoProvider>,
    certified_key: Arc<CertifiedKey>,
) -> Result<ClientConfig, rustls::Error> {
    Ok(ClientConfig::builder_with_provider(Arc::clone(provider))
        // TLS 1.3 and 1.2, which is what "safe defaults" means for a `tls12` build:
        // Android 8 and 9 offer no 1.3, and refusing those phones buys nothing the pin
        // does not already give (§11.3).
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyPhone {
            provider: Arc::clone(provider),
        }))
        .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key))))
}

/// The `ServerConfig` of §11.4, for the half of the upload listener that speaks TLS.
///
/// It is the mirror image of [`build_pairing_client_config`] and shorter for a reason:
/// as the server, this host verifies nothing and offers everything.
///
/// - **No client certificate is requested** — [`with_no_client_auth`] rather than a
///   verifier that accepts anything. The app has no identity this host could check, and
///   what authenticates an upload is the token; asking for a certificate would only
///   produce `CertificateRequest`s the app answers empty and handshake failures to
///   explain. It is also the reason there is no `ClientCertVerifier` anywhere in this
///   crate outside its tests.
/// - **The same [`SingleCertAndKey`], over the same `Arc<CertifiedKey>`** as the pairing
///   config. This is the whole of §11.2 in one line: the phone pinned the SHA-256 of the
///   certificate it was shown while pairing, and it refuses this connection — permanently,
///   with no prompt and no way to click through — if a different one arrives.
///
/// [`with_no_client_auth`]: rustls::ConfigBuilder::with_no_client_auth
fn build_upload_server_config(
    provider: &Arc<CryptoProvider>,
    certified_key: Arc<CertifiedKey>,
) -> Result<ServerConfig, rustls::Error> {
    Ok(ServerConfig::builder_with_provider(Arc::clone(provider))
        // 1.3 and 1.2, for the same reason as the client config: an Android 8 or 9 phone
        // that paired over 1.2 has to be able to upload over it too (§11.3).
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key))))
}

/// Accepts any certificate a phone presents while pairing. See
/// [`build_pairing_client_config`] for why, and keep this private so that "which configs
/// can reach it" stays a one-line answer.
#[derive(Debug)]
struct AcceptAnyPhone {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyPhone {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    // The two below are *not* waved through with the identity. They check that the peer
    // holds the private key for the certificate it just presented, which is a property of
    // the handshake rather than a claim about who the phone is, and it is what stops a
    // machine in the middle from replaying somebody else's certificate into this
    // connection. Delegating to the provider's own implementation is also how they stay
    // correct across a rustls upgrade.
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

fn write_new(path: &Path, contents: &[u8], mode: u32) -> Result<(), IdentityError> {
    let write_error = |source| IdentityError::Write {
        path: path.to_path_buf(),
        source,
    };

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(write_error)?;
    file.write_all(contents).map_err(write_error)?;
    file.sync_all().map_err(write_error)
}

fn sync_dir(dir: &Path) -> Result<(), IdentityError> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| IdentityError::Write {
            path: dir.to_path_buf(),
            source,
        })
}

/// Why the host has no TLS identity. Every variant is a startup failure the caller
/// reports and then continues without TLS — none of them is a reason to generate a new
/// key (§11.5).
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("{0}")]
    Directory(String),
    #[error("cannot generate a mobile TLS identity: {0}")]
    Generate(rcgen::Error),
    #[error("cannot write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: rustls::pki_types::pem::Error,
    },
    #[error("the mobile TLS certificate and key are not a usable pair: {0}")]
    Unusable(rustls::Error),
    #[error(
        "{} exists but {} does not; restore it, or remove both to start a new identity \
		 — which unpairs every phone paired over TLS",
        present.display(),
        missing.display()
    )]
    Incomplete { present: PathBuf, missing: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("generated file exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn generates_a_p256_identity_on_first_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = HostIdentity::load_or_generate(dir.path()).expect("first start generates");

        assert_eq!(mode_of(&dir.path().join(KEY_FILE)), 0o600);
        assert_eq!(identity.fingerprint_sha256().len(), 64);
        assert!(
            identity
                .fingerprint_sha256()
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            identity.certified_key().key.algorithm(),
            rustls::SignatureAlgorithm::ECDSA
        );
    }

    /// The load-bearing one: §11.2's pin survives a restart only if the second start
    /// presents the certificate the first start wrote.
    #[test]
    fn a_second_start_loads_the_same_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = HostIdentity::load_or_generate(dir.path()).expect("first start generates");
        let second = HostIdentity::load_or_generate(dir.path()).expect("second start loads");

        assert_eq!(first.fingerprint_sha256(), second.fingerprint_sha256());
        assert_eq!(first.certificate(), second.certificate());
    }

    #[test]
    fn an_unparsable_key_is_an_error_and_not_a_regeneration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = HostIdentity::load_or_generate(dir.path()).expect("first start generates");
        let key_path = dir.path().join(KEY_FILE);
        let before = fs::read(&key_path).expect("key is readable");

        fs::write(&key_path, b"-----BEGIN PRIVATE KEY-----\nnot base64\n").expect("clobber key");
        let error = HostIdentity::load_or_generate(dir.path()).expect_err("must not regenerate");

        assert!(matches!(error, IdentityError::Parse { .. }), "{error:?}");
        assert_ne!(fs::read(&key_path).expect("key still there"), before);
        assert_eq!(
            HostIdentity::load_or_generate(dir.path())
                .err()
                .map(|error| matches!(error, IdentityError::Parse { .. })),
            Some(true)
        );
        // And the certificate the phone pinned was left exactly where it was.
        assert_eq!(
            crate::hex_lower(&Sha256::digest(
                CertificateDer::from_pem_slice(
                    &fs::read(dir.path().join(CERT_FILE)).expect("cert is readable")
                )
                .expect("cert still parses")
                .as_ref()
            )),
            identity.fingerprint_sha256()
        );
    }

    #[test]
    fn a_key_whose_certificate_is_gone_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        HostIdentity::load_or_generate(dir.path()).expect("first start generates");
        fs::remove_file(dir.path().join(CERT_FILE)).expect("remove cert");

        let error = HostIdentity::load_or_generate(dir.path()).expect_err("must not regenerate");

        assert!(
            matches!(error, IdentityError::Incomplete { .. }),
            "{error:?}"
        );
        assert!(dir.path().join(KEY_FILE).exists(), "key was left alone");
    }

    /// §11.3's warning about client libraries that only answer for a recognised CA,
    /// asserted where it can be asserted without a phone: the phone's
    /// `CertificateRequest` carries no `certificate_authorities`, which reaches the
    /// resolver as an empty hint list, and the certificate must go out anyway.
    #[test]
    fn the_pairing_config_sends_our_certificate_against_an_empty_ca_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = HostIdentity::load_or_generate(dir.path()).expect("first start generates");

        let resolved = identity
            .pairing_client_config()
            .client_auth_cert_resolver
            .resolve(&[], &[rustls::SignatureScheme::ECDSA_NISTP256_SHA256])
            .expect("a CertificateRequest with no acceptable issuers is still answered");

        // The same certificate, not merely an equivalent one: this is the pin (§11.2).
        assert_eq!(resolved.cert, identity.certified_key().cert);
    }

    /// A key that does not belong to the certificate is caught at load, not at the first
    /// handshake weeks later.
    #[test]
    fn a_mismatched_key_is_rejected_at_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let other = tempfile::tempdir().expect("tempdir");
        HostIdentity::load_or_generate(dir.path()).expect("first start generates");
        HostIdentity::load_or_generate(other.path()).expect("a second, unrelated identity");

        fs::copy(other.path().join(KEY_FILE), dir.path().join(KEY_FILE)).expect("swap the key");
        let error = HostIdentity::load_or_generate(dir.path()).expect_err("mismatch is fatal");

        assert!(matches!(error, IdentityError::Unusable(_)), "{error:?}");
    }
}
