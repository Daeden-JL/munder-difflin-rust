//! Server entry point.

use std::path::PathBuf;
use std::sync::Arc;

use md_server::auth::Account;
use md_server::state::ServerConfig;
use md_tenant::sandbox::{ContainerSandbox, PassthroughSandbox};
use md_tenant::{Sandbox, TenantId};

/// Container healthcheck: probe `/api/health` over a plain TCP request and exit
/// 0/1. Written by hand rather than with an HTTP client so the runtime image
/// needs neither curl nor a TLS stack for what is a loopback GET.
fn health_check(bind: &str) -> std::process::ExitCode {
    use std::io::{Read, Write};
    // The server may bind 0.0.0.0; the probe still connects over loopback.
    let port = bind.rsplit(':').next().unwrap_or("7777");
    let addr = format!("127.0.0.1:{port}");
    let timeout = std::time::Duration::from_secs(2);

    let Ok(sock) = addr.parse::<std::net::SocketAddr>() else {
        return std::process::ExitCode::FAILURE;
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&sock, timeout) else {
        return std::process::ExitCode::FAILURE;
    };
    let _ = stream.set_read_timeout(Some(timeout));

    // Under TLS a plaintext GET would be rejected at the handshake, so the probe
    // degrades to "the listener accepted a connection". That is liveness, not
    // readiness — bundling a TLS client into the image to learn slightly more is
    // not worth the dependency for a healthcheck.
    if std::env::var("MD_TLS_CERT").is_ok() {
        return std::process::ExitCode::SUCCESS;
    }
    let req = "GET /api/health HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(req.as_bytes()).is_err() {
        return std::process::ExitCode::FAILURE;
    }
    let mut buf = String::new();
    if stream.read_to_string(&mut buf).is_err() {
        return std::process::ExitCode::FAILURE;
    }
    if buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200") {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    let bind_env = std::env::var("MD_BIND").unwrap_or_else(|_| "127.0.0.1:7777".into());
    if std::env::args().any(|a| a == "--health-check") {
        return Ok(health_check(&bind_env));
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "md_server=info,tower_http=info".into()),
        )
        .init();

    let cfg = ServerConfig {
        bind: bind_env,
        data_root: std::env::var("MD_DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./data")),
        static_dir: std::env::var("MD_STATIC_DIR").ok().map(PathBuf::from),
    };

    let sandbox: Arc<dyn Sandbox> = match std::env::var("MD_SANDBOX").as_deref() {
        Ok("container") => Arc::new(ContainerSandbox {
            runtime: std::env::var("MD_CONTAINER_RUNTIME").unwrap_or_else(|_| "podman".into()),
            image: std::env::var("MD_AGENT_IMAGE")
                .unwrap_or_else(|_| "munderdifflin/agent:0.4.6".into()),
        }),
        // Defaulting to no isolation would be the wrong default for a server, so
        // it must be asked for by name.
        Ok("passthrough") => Arc::new(PassthroughSandbox),
        _ => {
            tracing::warn!(
                "MD_SANDBOX unset; defaulting to passthrough (single tenant only)"
            );
            Arc::new(PassthroughSandbox)
        }
    };

    // Bootstrap account for local development. A real deployment loads accounts
    // from the control-plane store; this exists so `cargo run` works.
    let accounts = vec![Account::new(
        &std::env::var("MD_USER").unwrap_or_else(|_| "dev".into()),
        TenantId::parse(&std::env::var("MD_TENANT").unwrap_or_else(|_| "dev".into()))
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        &std::env::var("MD_PASSWORD").unwrap_or_else(|_| "dev".into()),
    )?];

    let app = md_server::build(&cfg, accounts, sandbox)?;

    let tls = match (std::env::var("MD_TLS_CERT").ok(), std::env::var("MD_TLS_KEY").ok()) {
        (Some(cert), Some(key)) => Some((cert, key)),
        (None, None) => None,
        // A half-configured pair means someone intended TLS and mistyped. Serving
        // plaintext anyway would silently deliver the opposite of the intent.
        _ => anyhow::bail!("MD_TLS_CERT and MD_TLS_KEY must be set together"),
    };

    let ported = md_contract::Rpc::ALL.len() - md_server::rpc::unported().len();
    let total = md_contract::Rpc::ALL.len();

    match tls {
        Some((cert, key)) => {
            let addr: std::net::SocketAddr = cfg.bind.parse()?;
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .map_err(|e| anyhow::anyhow!("loading TLS cert {cert} / key {key}: {e}"))?;
            tracing::info!(bind = %cfg.bind, tls = true, ported, total,
                "munder difflin server listening");
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            // Warn only when reachable beyond loopback: plaintext on 127.0.0.1 is
            // fine, plaintext on a LAN address is a credential on the wire.
            if !cfg.bind.starts_with("127.") && !cfg.bind.starts_with("localhost") {
                tracing::warn!(bind = %cfg.bind,
                    "serving PLAINTEXT on a non-loopback address; set MD_TLS_CERT/MD_TLS_KEY");
            }
            let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
            tracing::info!(bind = %cfg.bind, tls = false, ported, total,
                "munder difflin server listening");
            axum::serve(listener, app).await?;
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}
