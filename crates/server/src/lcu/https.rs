//! From irelia-companion's `server/lcu/https.ts`: the League client and the Live Client API serve a Riot self-signed certificate on
//! 127.0.0.1 only. Certificate checks are off for these loopback requests exclusively (never for anything else).

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

static LOOPBACK: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .expect("loopback client")
});

fn url(port: u16, path: &str) -> String {
    format!("https://127.0.0.1:{port}{path}")
}

pub async fn loopback_json(port: u16, path: &str, auth: Option<&str>, timeout_ms: u64) -> Result<Value> {
    let mut req = LOOPBACK.get(url(port, path)).header("Accept", "application/json").timeout(Duration::from_millis(timeout_ms));
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let res = req.send().await?;
    let status = res.status().as_u16();
    let text = res.text().await?;
    if status >= 400 {
        return Err(anyhow!("HTTP {status} {path}"));
    }
    Ok(serde_json::from_str(&text)?)
}

/// A write to the League client (the import). An empty answer (204) is Null.
pub async fn loopback_request(port: u16, method: &str, path: &str, auth: &str, body: Option<&Value>, timeout_ms: u64) -> Result<Value> {
    let m = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut req = LOOPBACK.request(m, url(port, path)).header("Accept", "application/json").header("Authorization", auth).timeout(Duration::from_millis(timeout_ms));
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(bf_core::js::stringify(b));
    }
    let res = req.send().await?;
    let status = res.status().as_u16();
    let text = res.text().await?;
    if status >= 400 {
        let detail = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(String::from)).unwrap_or_default();
        return Err(anyhow!("HTTP {status} {method} {path}{}", if detail.is_empty() { String::new() } else { format!(": {detail}") }));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&text)?)
}

/// rustls without certificate checks, for the League client's loopback websocket only.
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub fn loopback_tls() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
        .with_no_client_auth();
    Arc::new(cfg)
}
