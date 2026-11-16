//! TLS for the WhatsApp socket and the HTTP client.
//!
//! Why: whatsapp-rust-tokio-transport trusts the bundled Mozilla roots only. On a network
//! that re-signs HTTPS with a private root installed in the OS store, the handshake fails with
//! `UnknownIssuer`. The WebSocket connector here trusts Mozilla roots AND the OS roots; the
//! ureq client (version lookup, media) uses the OS verifier.

use std::sync::Arc;

use chatcli_core::{log, Result};
use rustls::{ClientConfig, RootCertStore};
use whatsapp_rust::http::UreqHttpClient;
use whatsapp_rust::transport::Connector;

/// Mozilla roots plus every parsable OS root. Returns the store and how many OS roots were
/// added (0 means the OS store could not be read: only public roots are trusted).
fn root_store() -> (RootCertStore, usize) {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        log::error(
            "whatsapp",
            &format!("reading an OS root certificate failed: {e}"),
        );
    }
    let (added, _ignored) = roots.add_parsable_certificates(native.certs);
    (roots, added)
}

/// TLS connector for the WebSocket transport.
pub fn ws_connector() -> Result<Connector> {
    let (roots, os_roots) = root_store();
    log::info(
        "whatsapp",
        &format!("tls roots: webpki + {os_roots} OS roots"),
    );
    // An explicit provider: other crates in the workspace link rustls with aws-lc-rs too, and
    // `ClientConfig::builder()` panics when two providers are present and none is installed.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("WhatsApp TLS setup failed ({e}): update chatcli"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Connector::Rustls(tokio_rustls::TlsConnector::from(
        Arc::new(config),
    )))
}

/// HTTP client for the version lookup and media, verifying through the OS.
pub fn http_client() -> UreqHttpClient {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let agent: ureq::Agent = ureq::config::Config::builder()
        .tls_config(tls)
        .build()
        .into();
    UreqHttpClient::with_agent(agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_store_holds_public_roots() {
        crate::test_data_dir();
        let (roots, _os) = root_store();
        // The Mozilla bundle alone is well over a hundred roots.
        assert!(roots.len() > 100, "only {} roots", roots.len());
    }

    #[test]
    fn connector_builds() {
        crate::test_data_dir();
        assert!(matches!(ws_connector(), Ok(Connector::Rustls(_))));
    }
}
