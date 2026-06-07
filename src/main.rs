mod api;
mod certs;
mod config;
mod control_plane;
mod core;
mod diff;
mod export;
mod har;
mod middleware;
mod redaction;
mod runtime;
mod security;
mod session;
mod setup;
mod storage;
mod telemetry;
mod transport;
mod webhooks;

pub(crate) use runtime::AppState;

#[tokio::main]
async fn main() -> Result<(), runtime::StartupError> {
    // rustls 0.23 cannot auto-select a CryptoProvider when more than one backend
    // is compiled in (both aws-lc-rs and ring are, pulled by reqwest, quinn/h3 and
    // tokio-tungstenite). Without this it panics the first time any TLS config is
    // built (MITM, the HTTPS listener, wss upstream, h3). Install one explicitly
    // before anything touches TLS. `install_default` only fails if already set, so
    // the error is ignored.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    runtime::run().await
}
