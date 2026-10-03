//! TLS for the IMAP connections and the HTTPS agent, behind whichever backend
//! the build picked: `native-tls` (the platform's library) or `rustls` (with
//! the crypto provider the program installed as the process default). SMTP
//! goes through lettre, which the same features configure.
//!
//! If both features are on (`--all-features`), rustls wins.

#[cfg(not(any(feature = "native-tls", feature = "rustls")))]
compile_error!("esmail needs a TLS backend: enable the `native-tls` or the `rustls` feature");

use tokio::net::TcpStream;

#[cfg(feature = "rustls")]
pub type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;
#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
pub type TlsStream = tokio_native_tls::TlsStream<TcpStream>;

/// Opens a TCP connection to `host:port` and completes a TLS handshake with
/// the server's certificate verified for `host`.
pub async fn connect(host: &str, port: u16) -> anyhow::Result<TlsStream> {
    let stream = TcpStream::connect((host, port)).await?;
    handshake(host, stream).await
}

#[cfg(feature = "rustls")]
async fn handshake(host: &str, stream: TcpStream) -> anyhow::Result<TlsStream> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
    let connector = tokio_rustls::TlsConnector::from(rustls_backend::client_config()?);
    Ok(connector.connect(server_name, stream).await?)
}

#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
async fn handshake(host: &str, stream: TcpStream) -> anyhow::Result<TlsStream> {
    let connector = tokio_native_tls::native_tls::TlsConnector::builder().build()?;
    Ok(tokio_native_tls::TlsConnector::from(connector).connect(host, stream).await?)
}

/// The TLS settings for a `ureq` agent: with rustls, the same trust anchors
/// as the IMAP connections (ureq would otherwise look for its built-in
/// `webpki-roots`, which this build leaves out); otherwise ureq's defaults.
pub fn http_config() -> anyhow::Result<ureq::tls::TlsConfig> {
    #[cfg(feature = "rustls")]
    {
        let roots: Vec<ureq::tls::Certificate<'static>> = rustls_backend::roots()?
            .iter()
            .map(|der| ureq::tls::Certificate::from_der(der.as_ref()).to_owned())
            .collect();
        Ok(ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::new_with_certs(&roots))
            .build())
    }
    #[cfg(not(feature = "rustls"))]
    {
        Ok(ureq::tls::TlsConfig::default())
    }
}

#[cfg(feature = "rustls")]
mod rustls_backend {
    use std::sync::{Arc, OnceLock};

    use anyhow::Context;
    use rustls::pki_types::CertificateDer;

    /// The system's trust anchors, read once: the CA bundle is a few hundred
    /// KiB of PEM, and every connection of every account needs it.
    pub(super) fn roots() -> anyhow::Result<&'static [CertificateDer<'static>]> {
        static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
        if let Some(roots) = ROOTS.get() {
            return Ok(roots);
        }
        let loaded = rustls_native_certs::load_native_certs();
        if loaded.certs.is_empty() {
            // A missing bundle must never mean "trust nothing checked".
            anyhow::bail!("no trusted CA certificates found ({:?})", loaded.errors);
        }
        Ok(ROOTS.get_or_init(|| loaded.certs))
    }

    pub(super) fn client_config() -> anyhow::Result<Arc<rustls::ClientConfig>> {
        static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
        if let Some(config) = CONFIG.get() {
            return Ok(Arc::clone(config));
        }
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .context("no rustls crypto provider installed (CryptoProvider::install_default)")?;
        let mut store = rustls::RootCertStore::empty();
        let (added, _ignored) = store.add_parsable_certificates(roots()?.iter().cloned());
        anyhow::ensure!(added > 0, "none of the system's CA certificates could be parsed");
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(store)
            .with_no_client_auth();
        Ok(Arc::clone(CONFIG.get_or_init(|| Arc::new(config))))
    }
}
