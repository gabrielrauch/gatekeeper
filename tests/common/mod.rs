use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tokio::net::TcpListener;

use gatekeeper::config::{
    Config, DefaultsConfig, FailMode, MemoryStoreConfig, ProxyConfig, ServerConfig, StoreConfig,
};

/// Spawn a mock upstream server.
/// Routes:
///   GET  /            → 200 "upstream-ok"
///   POST /echo        → 200 echo body
///   GET  /status/201  → 201 empty body
///
/// Returns the base URL of the mock upstream.
pub async fn spawn_mock_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let app = Router::new()
        .route("/", get(|| async { (StatusCode::OK, "upstream-ok") }))
        .route(
            "/echo",
            post(|req: Request| async move {
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                (StatusCode::OK, Body::from(body))
            }),
        )
        .route(
            "/status/201",
            get(|| async { StatusCode::CREATED.into_response() }),
        );

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{addr}")
}

/// Spawn a full Gatekeeper instance with configurable rate limiting.
/// Returns the SocketAddr of the bound listener.
pub async fn spawn_gatekeeper(
    upstream_url: String,
    capacity: u64,
    refill_rate: f64,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = Arc::new(Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream_url,
            proxy: ProxyConfig::default(),
        },
        defaults: DefaultsConfig {
            algorithm: "token_bucket".to_string(),
            capacity,
            refill_rate,
            cost: 1,
            fail_mode: FailMode::Open,
        },
        store: StoreConfig {
            memory: MemoryStoreConfig::default(),
        },
    });

    let app = gatekeeper::server::build_app(config);

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    addr
}

/// Spawn a gatekeeper proxy pointed at `upstream_url`.
/// Uses high capacity to effectively disable rate limiting.
/// Returns the base URL of the proxy.
pub async fn spawn_gatekeeper_proxy_only(upstream_url: String) -> String {
    let addr = spawn_gatekeeper(upstream_url, 100_000, 100_000.0).await;
    format!("http://{addr}")
}
