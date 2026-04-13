use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use hyper::Request;
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::trace::TraceLayer;

use crate::algorithm::token_bucket::TokenBucket;
use crate::config::Config;
use crate::identity::ip::IpExtractor;
use crate::metrics::{init_prometheus, MetricsLayer};
use crate::middleware::rate_limit::RateLimitLayer;
use crate::proxy::http::HttpProxy;
use crate::proxy::ProxyHandler;
use crate::response::ResponseHeaderLayer;
use crate::store::memory::MemoryStore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub proxy: Arc<HttpProxy>,
    pub store: Arc<MemoryStore>,
    pub prometheus_handle: PrometheusHandle,
}

pub fn build_app(config: Arc<Config>) -> Router {
    // 1. Create proxy
    let proxy = Arc::new(HttpProxy::new(config.server.upstream_url.clone()));

    // 2. Create memory store
    let store = Arc::new(MemoryStore::new(config.store.memory.max_entries));

    // 3. Init prometheus
    let prometheus_handle = init_prometheus();

    // 4. Start TTL eviction task
    let ttl_secs = ((config.defaults.capacity as f64 / config.defaults.refill_rate) * 2.0)
        .max(60.0);
    let ttl = Duration::from_secs_f64(ttl_secs);
    let eviction_interval = config.store.memory.eviction_interval;
    store.start_eviction_task(eviction_interval, ttl);

    // 5. Create TokenBucket limiter
    let limiter = Arc::new(TokenBucket::new(
        Arc::clone(&store),
        config.defaults.capacity,
        config.defaults.refill_rate,
    ));

    // 6. Create IpExtractor
    let extractor = Arc::new(IpExtractor);

    // 7. Create RateLimitLayer
    let rate_limit_layer = RateLimitLayer::new(
        limiter,
        extractor,
        config.defaults.fail_mode.clone(),
        config.defaults.cost,
    );

    let state = AppState {
        config,
        proxy,
        store,
        prometheus_handle,
    };

    // 8. Build proxy router with rate limiting.
    // Layer order: rate_limit_layer is applied first (inner), ResponseHeaderLayer is outer.
    // On the response path: rate_limit inserts Decision into extensions, then
    // ResponseHeaderLayer reads it and injects x-ratelimit-* headers.
    let proxy_router: Router<AppState> = Router::new()
        .fallback(proxy_handler)
        .layer(rate_limit_layer)
        .layer(ResponseHeaderLayer);

    // 9. Build main router: health + metrics bypass rate limiting, proxy is rate-limited
    Router::new()
        .route("/healthz", get(crate::health::healthz))
        .route("/readyz", get(crate::health::readyz))
        .route("/metrics", get(metrics_handler))
        .merge(proxy_router)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(MetricsLayer)
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<axum::body::Body>,
) -> impl IntoResponse {
    let start = std::time::Instant::now();
    match state.proxy.proxy(req, Some(addr.ip())).await {
        Ok(response) => {
            let elapsed = start.elapsed();
            metrics::histogram!("gatekeeper_upstream_duration_seconds")
                .record(elapsed.as_secs_f64());
            response.into_response()
        }
        Err(e) => {
            tracing::error!("proxy error: {e}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

pub async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let bucket_count = state.store.bucket_count() as f64;
    metrics::gauge!("gatekeeper_active_buckets").set(bucket_count);
    state.prometheus_handle.render()
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
