//! The HTTPS client providers talk through.
//!
//! TLS is set up here and not taken from the process: bundled roots and a
//! named crypto provider, so the static binary behaves the same on every
//! server, whatever is installed there.

use std::sync::Arc;
use std::time::Duration;

fn tls_config() -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn builder(
    tls: rustls::ClientConfig,
    connect: Duration,
    total: Duration,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(connect)
        .timeout(total)
        .user_agent(concat!("vpush/", env!("CARGO_PKG_VERSION")))
}

/// The client of a service that speaks HTTP/1.1. A TLS config handed over
/// as it is gets no ALPN from reqwest, so every connection is HTTP/1.1.
pub fn client(connect: Duration, total: Duration) -> Result<reqwest::Client, String> {
    builder(tls_config()?, connect, total)
        .build()
        .map_err(|e| e.to_string())
}

/// The client of a service that speaks HTTP/2 alone: APNs. The TLS config
/// offers `h2` and nothing else, and the client speaks HTTP/2 from the
/// first byte; over plain `http://`, in the tests, that is h2c.
pub fn h2_client(connect: Duration, total: Duration) -> Result<reqwest::Client, String> {
    let mut tls = tls_config()?;
    tls.alpn_protocols = vec![b"h2".to_vec()];
    builder(tls, connect, total)
        .http2_prior_knowledge()
        .build()
        .map_err(|e| e.to_string())
}

/// `Retry-After` in its seconds form. The date form is rare with push
/// services and is read as absent.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_without_a_process_default_provider() {
        client(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
        h2_client(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
    }
}
