use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use hyper::Request;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::proxy::http::HttpProxy;
use crate::proxy::ProxyHandler;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub proxy: Arc<HttpProxy>,
}

pub fn build_app(config: Arc<Config>) -> Router {
    let proxy = Arc::new(HttpProxy::new(config.server.upstream_url.clone()));
    let state = AppState { config, proxy };

    Router::new()
        .route("/healthz", get(crate::health::healthz))
        .route("/readyz", get(crate::health::readyz))
        .fallback(proxy_handler)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<axum::body::Body>,
) -> impl IntoResponse {
    match state.proxy.proxy(req, Some(addr.ip())).await {
        Ok(response) => response.into_response(),
        Err(e) => {
            tracing::error!("proxy error: {e}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

pub async fn run(config: Config) {
    let listen = config.server.listen;
    let config = Arc::new(config);
    let app = build_app(config);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("failed to bind TCP listener");

    tracing::info!("Gatekeeper listening on {listen}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server error");
}
