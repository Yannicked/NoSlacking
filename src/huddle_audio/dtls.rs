//! The DTLS for the media connection: OpenSSL's, through str0m, with
//! what the handshake settled on written to the log.
//!
//! Chime's media servers refused the handshake of dimpl, the DTLS every
//! other str0m backend uses (alert 40, handshake_failure), and took
//! OpenSSL's. Which DTLS version, SRTP profile and server key it came to
//! is logged once per connection, so a later try of dimpl knows what it
//! has to offer. The cipher suite stays inside str0m-openssl, which does
//! not hand it out.

use std::sync::OnceLock;
use std::time::Instant;

use str0m::crypto::CryptoProvider;
use str0m::crypto::dtls::{
    DtlsCert, DtlsImplError, DtlsInstance, DtlsOutput, DtlsProvider, DtlsVersion, ProtocolVersion,
};

/// The crypto for a peer: OpenSSL's, its DTLS watched for the log.
pub fn provider() -> CryptoProvider {
    let mut provider = str0m::crypto::from_feature_flags();
    static WATCHED: OnceLock<&'static Watched> = OnceLock::new();
    let inner = provider.dtls_provider;
    let watched: &'static Watched = WATCHED.get_or_init(|| Box::leak(Box::new(Watched { inner })));
    provider.dtls_provider = watched;
    provider
}

/// A DTLS provider that hands out [`Watching`] instances of another's.
#[derive(Debug)]
struct Watched {
    inner: &'static dyn DtlsProvider,
}

impl DtlsProvider for Watched {
    fn generate_certificate(&self) -> Option<DtlsCert> {
        self.inner.generate_certificate()
    }

    fn new_dtls(
        &self,
        cert: &DtlsCert,
        now: Instant,
        dtls_version: DtlsVersion,
        mtu: Option<usize>,
    ) -> Result<Box<dyn DtlsInstance>, str0m::crypto::CryptoError> {
        let inner = self.inner.new_dtls(cert, now, dtls_version, mtu)?;
        Ok(Box::new(Watching { inner }))
    }
}

/// One DTLS connection, passed through untouched but for a look at the
/// server's certificate and the SRTP profile on their way by.
#[derive(Debug)]
struct Watching {
    inner: Box<dyn DtlsInstance>,
}

impl DtlsInstance for Watching {
    fn set_active(&mut self, active: bool) {
        self.inner.set_active(active);
    }

    fn handle_packet(&mut self, packet: &[u8]) -> Result<(), DtlsImplError> {
        self.inner.handle_packet(packet)
    }

    fn poll_output<'a>(&mut self, buf: &'a mut [u8]) -> DtlsOutput<'a> {
        let output = self.inner.poll_output(buf);
        match &output {
            DtlsOutput::PeerCert(der) => {
                log::info!(
                    "connect: DTLS {}; the media server's key is {}",
                    version(self.inner.protocol_version()),
                    key_of(der)
                );
            }
            DtlsOutput::KeyingMaterial(_, profile) => {
                log::info!("connect: SRTP profile {profile:?}");
            }
            _ => {}
        }
        output
    }

    fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsImplError> {
        self.inner.handle_timeout(now)
    }

    fn send_application_data(&mut self, data: &[u8]) -> Result<(), DtlsImplError> {
        self.inner.send_application_data(data)
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.inner.protocol_version()
    }

    fn is_closing(&self) -> bool {
        self.inner.is_closing()
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    fn close(&mut self) -> Result<(), DtlsImplError> {
        self.inner.close()
    }
}

/// The DTLS version for the log.
fn version(version: Option<ProtocolVersion>) -> String {
    version.map_or_else(|| "(version not known)".to_owned(), |v| format!("{v:?}"))
}

/// The kind and size of the key in the DER certificate `der`, such as
/// "EC P-256 (256 bits)" or "RSA (2048 bits)".
pub fn key_of(der: &[u8]) -> String {
    use openssl::pkey::Id;
    let Ok(key) = openssl::x509::X509::from_der(der).and_then(|cert| cert.public_key()) else {
        return "unreadable".to_owned();
    };
    let bits = key.bits();
    let kind = match key.id() {
        Id::RSA => "RSA".to_owned(),
        Id::EC => {
            let curve = key
                .ec_key()
                .ok()
                .and_then(|ec| ec.group().curve_name())
                .and_then(|nid| nid.short_name().ok().map(str::to_owned));
            match curve {
                Some(curve) => format!("EC {curve}"),
                None => "EC".to_owned(),
            }
        }
        Id::ED25519 => "Ed25519".to_owned(),
        other => format!("type {}", other.as_raw()),
    };
    format!("{kind} ({bits} bits)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watched_provider_makes_certificates_and_names_their_key() {
        let provider = provider();
        let cert = provider
            .dtls_provider
            .generate_certificate()
            .expect("OpenSSL makes a certificate");
        let key = key_of(&cert.certificate);
        assert!(key.starts_with("EC ") || key.starts_with("RSA "), "{key}");
        assert!(key.ends_with(" bits)"), "{key}");
        assert_eq!(key_of(b"not a certificate"), "unreadable");
        // Asked twice, the same watcher.
        let again = super::provider();
        assert!(std::ptr::addr_eq(
            provider.dtls_provider,
            again.dtls_provider
        ));
    }
}
