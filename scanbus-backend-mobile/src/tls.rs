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
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
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
        let certified_key = CertifiedKey::from_der(
            vec![certificate],
            key,
            &rustls::crypto::ring::default_provider(),
        )
        .map_err(IdentityError::Unusable)?;

        Ok(Self {
            certified_key: Arc::new(certified_key),
            fingerprint,
        })
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
