//! TLS configuration shared by Lambo's reqwest clients.

use std::sync::{Arc, OnceLock};

pub(crate) fn builder() -> reqwest::ClientBuilder {
    static TLS: OnceLock<rustls::ClientConfig> = OnceLock::new();
    let tls = TLS.get_or_init(|| {
        let provider = rustls::crypto::ring::default_provider();
        // A host embedding Lambo may already have installed a provider.
        // These clients always select ring explicitly either way.
        let _ = provider.clone().install_default();
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth()
    });
    reqwest::Client::builder().tls_backend_preconfigured(tls.clone())
}

#[cfg(test)]
pub(crate) fn test_client() -> reqwest::Client {
    builder().build().expect("test HTTP client")
}
