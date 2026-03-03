use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// SHA-256 fingerprint of a DER-encoded certificate.
pub type CertHash = [u8; 32];

/// Generate a self-signed certificate for node-to-node QUIC transport.
///
/// Returns the DER-encoded certificate, its PKCS8 private key, and a SHA-256 fingerprint.
pub fn generate_self_signed_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>, CertHash) {
    let certified_key = rcgen::generate_simple_self_signed(vec!["rebar-node".to_string()])
        .expect("certificate generation failed");

    let cert_der = certified_key.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(certified_key.key_pair.serialize_der());

    let hash = cert_fingerprint(&cert_der);

    (cert_der, PrivateKeyDer::Pkcs8(key_der), hash)
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> CertHash {
    let mut hasher = Sha256::new();
    hasher.update(cert.as_ref());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_cert_returns_valid_cert() {
        let (cert, key, hash) = generate_self_signed_cert();
        assert!(!cert.is_empty());
        match &key {
            PrivateKeyDer::Pkcs8(k) => assert!(!k.secret_pkcs8_der().is_empty()),
            _ => panic!("expected PKCS8 key"),
        }
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn cert_fingerprint_is_deterministic() {
        let (cert, _, hash1) = generate_self_signed_cert();
        let hash2 = cert_fingerprint(&cert);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn different_certs_have_different_fingerprints() {
        let (_, _, hash1) = generate_self_signed_cert();
        let (_, _, hash2) = generate_self_signed_cert();
        assert_ne!(hash1, hash2);
    }
}
