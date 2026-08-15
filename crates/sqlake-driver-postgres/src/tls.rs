//! What each `sslmode` actually asks for.
//!
//! libpq's five modes are two decisions, not one: *encrypt or not*, and *check
//! the certificate or not*. `tokio-postgres` owns the first — it negotiates
//! TLS or refuses to — and rustls owns the second, so the mapping here is
//! where a mode stops being a word and becomes a policy.
//!
//! | mode | encrypted | chain | host name |
//! | --- | --- | --- | --- |
//! | `disable` | no | — | — |
//! | `prefer` | if offered | no | no |
//! | `require` | yes | no | no |
//! | `verify-ca` | yes | yes | no |
//! | `verify-full` | yes | yes | yes |
//!
//! `require` verifying nothing surprises people who read it as the strong
//! setting: it stops someone listening and not someone answering.

use std::sync::{Arc, OnceLock};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
};
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::profile::SslMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Encrypted and nothing more: `prefer` and `require`.
    None,
    /// The chain is checked, the host name is not: `verify-ca`.
    ChainOnly,
    /// Both: `verify-full`.
    Full,
}

impl Verification {
    /// What the mode asks of the certificate. `None` for `disable`, which has
    /// no certificate to ask about.
    #[must_use]
    pub const fn of(mode: SslMode) -> Option<Self> {
        Some(match mode {
            SslMode::Disable => return None,
            SslMode::Prefer | SslMode::Require => Self::None,
            SslMode::VerifyCa => Self::ChainOnly,
            SslMode::VerifyFull => Self::Full,
        })
    }
}

/// Loads the platform's trust store for the verifying modes, which is where a
/// company's own CA lives — the one an internal database is signed by.
pub fn client_config(verification: Verification) -> DriverResult<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let verifier: Arc<dyn ServerCertVerifier> = match verification {
        Verification::None => Arc::new(AcceptAnything(provider.clone())),
        Verification::ChainOnly => Arc::new(NameAgnostic(web_pki(&provider)?)),
        Verification::Full => web_pki(&provider)?,
    };

    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| DriverError::Connect(format!("setting up TLS: {err}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

fn web_pki(provider: &Arc<CryptoProvider>) -> DriverResult<Arc<WebPkiServerVerifier>> {
    WebPkiServerVerifier::builder_with_provider(native_roots()?, provider.clone())
        .build()
        .map_err(|err| DriverError::Connect(format!("setting up certificate checks: {err}")))
}

/// Reading it is blocking file or keychain I/O — on macOS a Security framework
/// query — and this runs inside `connect`, on a runtime worker. Once per
/// process is a pause nobody notices; once per connection would be a pause on
/// every one. A failure is not cached, so a machine that grows a trust store
/// later still gets one.
fn native_roots() -> DriverResult<Arc<RootCertStore>> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();

    if let Some(roots) = ROOTS.get() {
        return Ok(roots.clone());
    }

    let mut roots = RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    let (added, _) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        // Continuing with an empty trust store would fail every connection
        // with a certificate error, which reads as "the server is wrong"
        // rather than "this machine has no trust store".
        return Err(DriverError::Connect(format!(
            "no usable certificates in the system trust store ({} errors)",
            loaded.errors.len()
        )));
    }

    Ok(ROOTS.get_or_init(|| Arc::new(roots)).clone())
}

/// `verify-ca`: check the chain, ignore who the certificate says it is.
///
/// Implemented by delegating and forgiving exactly one error. Writing a second
/// chain validator to leave out the name check would mean maintaining a second
/// chain validator, and the one thing worse than skipping the host name is
/// skipping it *and* getting the rest subtly wrong.
#[derive(Debug)]
struct NameAgnostic(Arc<dyn ServerCertVerifier>);

impl ServerCertVerifier for NameAgnostic {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// `prefer` and `require`: encrypt, and believe whatever answers.
#[derive(Debug)]
struct AcceptAnything(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_mode_asks_for_what_its_name_says() {
        assert_eq!(Verification::of(SslMode::Disable), None);
        assert_eq!(Verification::of(SslMode::Prefer), Some(Verification::None));
        // `require` is encryption only. Reading it as the strong setting is
        // the classic misunderstanding, and it must not be encoded here.
        assert_eq!(Verification::of(SslMode::Require), Some(Verification::None));
        assert_eq!(
            Verification::of(SslMode::VerifyCa),
            Some(Verification::ChainOnly)
        );
        assert_eq!(
            Verification::of(SslMode::VerifyFull),
            Some(Verification::Full)
        );
    }

    /// An inner verifier that answers with whatever it was built with, so the
    /// wrapper can be tested without a certificate, a clock or a server.
    ///
    /// `ServerCertVerified` is deliberately unclonable — it is a token meaning
    /// "this was checked" — so the stub holds the refusal and mints the
    /// success itself.
    #[derive(Debug)]
    struct Fixed(Option<Error>);

    impl ServerCertVerifier for Fixed {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            match &self.0 {
                Some(err) => Err(err.clone()),
                None => Ok(ServerCertVerified::assertion()),
            }
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![SignatureScheme::ED25519]
        }
    }

    fn ask(inner: Option<Error>) -> Result<ServerCertVerified, Error> {
        let verifier = NameAgnostic(Arc::new(Fixed(inner)));
        verifier.verify_server_cert(
            &CertificateDer::from(vec![]),
            &[],
            &ServerName::try_from("db.internal").expect("a name"),
            &[],
            UnixTime::since_unix_epoch(std::time::Duration::from_secs(0)),
        )
    }

    #[test]
    fn verify_ca_forgives_the_name_and_nothing_else() {
        assert!(ask(None).is_ok());
        assert!(
            ask(Some(Error::InvalidCertificate(
                CertificateError::NotValidForName
            )))
            .is_ok()
        );

        // Everything else still has to hold. Forgiving expiry or an unknown
        // issuer here would make `verify-ca` mean `require` with extra steps.
        for refusal in [
            CertificateError::Expired,
            CertificateError::UnknownIssuer,
            CertificateError::BadSignature,
            CertificateError::Revoked,
        ] {
            assert!(
                ask(Some(Error::InvalidCertificate(refusal.clone()))).is_err(),
                "{refusal:?} should not be forgiven"
            );
        }
    }
}
