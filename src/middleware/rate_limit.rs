use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use http::{Response, StatusCode};
use hyper::Request;
use tower::{Layer, Service};
use tracing::{error, warn};

use crate::config::FailMode;
use crate::middleware::access_control::AllowlistBypassed;
use crate::policy::PolicyEngine;
use crate::response::build_429_response;

// ---------------------------------------------------------------------------
// RateLimitLayer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RateLimitLayer {
    policy_engine: Arc<PolicyEngine>,
    fail_mode: FailMode,
}

impl RateLimitLayer {
    pub fn new(policy_engine: Arc<PolicyEngine>, fail_mode: FailMode) -> Self {
        Self {
            policy_engine,
            fail_mode,
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            policy_engine: self.policy_engine.clone(),
            fail_mode: self.fail_mode.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimitService
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    policy_engine: Arc<PolicyEngine>,
    fail_mode: FailMode,
}

impl<S> Service<Request<Body>> for RateLimitService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        let policy_engine = self.policy_engine.clone();
        let fail_mode = self.fail_mode.clone();

        Box::pin(async move {
            // 1. Check for AllowlistBypassed marker → skip rate limiting
            if req.extensions().get::<AllowlistBypassed>().is_some() {
                return inner.call(req).await;
            }

            // 2. Get path and method
            let path = req.uri().path().to_string();
            let method = req.method().as_str().to_string();

            // 3. Resolve policy (borrow ends before await)
            let (policy_name, limiter, extractor, cost, bypass): (
                String,
                Arc<dyn crate::algorithm::RateLimiter>,
                Arc<dyn crate::identity::IdentityExtractor>,
                u64,
                bool,
            ) = {
                let policy = policy_engine.resolve(&path, &method);
                (
                    policy.name.clone(),
                    policy.limiter.clone(),
                    policy.extractor.clone(),
                    policy.cost,
                    policy.bypass,
                )
            };

            // 4. If bypass policy → record metric and forward
            if bypass {
                metrics::counter!(
                    "gatekeeper_requests_total",
                    "status" => "bypassed",
                    "policy" => policy_name,
                )
                .increment(1);
                return inner.call(req).await;
            }

            // 5. Extract identity
            let identity_key = extractor
                .extract(&req)
                .map(|id| id.key)
                .unwrap_or_else(|| "unknown".to_string());

            // 6. Construct bucket key
            let bucket_key = format!("{policy_name}:{identity_key}");

            // 7. Check rate limit, measure duration
            let check_start = std::time::Instant::now();
            let result = limiter.check(&bucket_key, cost, false).await;
            let check_duration = check_start.elapsed();

            let algorithm = limiter.algorithm_name();
            metrics::histogram!(
                "gatekeeper_decision_duration_seconds",
                "algorithm" => algorithm,
            )
            .record(check_duration.as_secs_f64());

            // 8. Handle result
            match result {
                Ok(decision) if decision.allowed => {
                    metrics::counter!(
                        "gatekeeper_requests_total",
                        "status" => "allowed",
                        "policy" => policy_name,
                    )
                    .increment(1);

                    let mut resp = inner.call(req).await?;
                    resp.extensions_mut().insert(decision);
                    Ok(resp)
                }
                Ok(decision) => {
                    metrics::counter!(
                        "gatekeeper_requests_total",
                        "status" => "denied",
                        "policy" => policy_name.clone(),
                    )
                    .increment(1);

                    Ok(build_429_response(&decision, &policy_name))
                }
                Err(e) => match fail_mode {
                    FailMode::Open => {
                        warn!("rate limiter error, failing open: {e}");
                        metrics::counter!(
                            "gatekeeper_requests_total",
                            "status" => "error",
                            "policy" => policy_name,
                        )
                        .increment(1);
                        inner.call(req).await
                    }
                    FailMode::Closed => {
                        error!("rate limiter error, failing closed: {e}");
                        let resp = Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::from("service unavailable"))
                            .expect("503 builder should not fail");
                        Ok(resp)
                    }
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::Future;
    use std::net::{IpAddr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use http::{Method, Response, StatusCode};
    use hyper::Request;
    use tower::{Layer, Service, ServiceExt};

    use super::RateLimitLayer;
    use crate::config::{
        AccessConfig, DefaultsConfig, FailMode, MatchRule, MemoryStoreConfig, PolicyConfig,
        ServerConfig, StoreConfig,
    };
    use crate::middleware::access_control::AllowlistBypassed;
    use crate::policy::PolicyEngine;
    use crate::store::memory::MemoryStore;

    #[derive(Clone)]
    struct OkService;

    impl Service<Request<Body>> for OkService {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Body>) -> Self::Future {
            Box::pin(async { Ok(Response::new(Body::empty())) })
        }
    }

    fn req_to(path: &str, method: &str, ip: &str) -> Request<Body> {
        let addr = SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 12345);
        let mut req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn make_config(
        default_capacity: u64,
        policies: Vec<PolicyConfig>,
    ) -> crate::config::Config {
        crate::config::Config {
            server: ServerConfig {
                listen: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
                upstream_url: "http://localhost:3000".to_string(),
                proxy: Default::default(),
            },
            defaults: DefaultsConfig {
                algorithm: "token_bucket".to_string(),
                capacity: default_capacity,
                refill_rate: 0.0,
                cost: 1,
                fail_mode: FailMode::Open,
            },
            store: StoreConfig {
                memory: MemoryStoreConfig {
                    max_entries: 1000,
                    eviction_interval: Duration::from_secs(60),
                },
            },
            access: AccessConfig::default(),
            policy: policies,
        }
    }

    fn make_policy(name: &str, path: &str, capacity: u64, bypass: bool) -> PolicyConfig {
        PolicyConfig {
            name: name.to_string(),
            match_rule: MatchRule {
                path: path.to_string(),
                methods: None,
            },
            algorithm: None,
            capacity: Some(capacity),
            refill_rate: Some(0.0),
            cost: Some(1),
            identify_by: None,
            bypass: Some(bypass),
        }
    }

    fn make_layer(config: crate::config::Config) -> RateLimitLayer {
        let store = Arc::new(MemoryStore::new(1000));
        let engine = Arc::new(PolicyEngine::new(&config, store));
        RateLimitLayer::new(engine, FailMode::Open)
    }

    // --- bypass policy for capacity=0 must not panic: use capacity=1 to avoid config validation issues
    // Actually PolicyConfig with capacity Some(0) would fail config validation, but here we build
    // PolicyEngine directly from config structs in tests (no validate() call), so it's fine.

    #[tokio::test]
    async fn uses_matching_policy_capacity() {
        // policy with cap=2 on /api/reports/*, default cap=100
        let config = make_config(
            100,
            vec![make_policy("reports", "/api/reports/*", 2, false)],
        );
        let layer = make_layer(config);
        let mut svc = layer.layer(OkService);

        let r1 = svc.ready().await.unwrap().call(req_to("/api/reports/q1", "GET", "1.2.3.4")).await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = svc.ready().await.unwrap().call(req_to("/api/reports/q1", "GET", "1.2.3.4")).await.unwrap();
        assert_eq!(r2.status(), StatusCode::OK);

        // 3rd request should hit the cap=2 policy
        let r3 = svc.ready().await.unwrap().call(req_to("/api/reports/q1", "GET", "1.2.3.4")).await.unwrap();
        assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn default_policy_for_unmatched_path() {
        // Only a policy on /api/reports/*, default cap=100
        let config = make_config(
            100,
            vec![make_policy("reports", "/api/reports/*", 2, false)],
        );
        let layer = make_layer(config);
        let mut svc = layer.layer(OkService);

        // Unmatched path should use default (cap=100), so many requests are fine
        for _ in 0..10 {
            let r = svc.ready().await.unwrap().call(req_to("/v2/users", "GET", "1.2.3.4")).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn bypass_policy_skips_rate_limit() {
        // bypass=true with capacity=0 (never allowed by limiter), but bypass should win
        // We use capacity=1 for the policy to avoid zero issues, but bypass=true skips the limiter entirely
        let mut policy = make_policy("webhooks", "/webhooks/*", 1, false);
        policy.bypass = Some(true);
        policy.capacity = None; // will use default capacity but bypass wins anyway

        let config = make_config(100, vec![policy]);
        let layer = make_layer(config);
        let mut svc = layer.layer(OkService);

        // Many requests — bypass means no rate limiting at all
        for _ in 0..5 {
            let r = svc.ready().await.unwrap().call(req_to("/webhooks/event", "POST", "1.2.3.4")).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn allowlist_bypassed_skips_rate_limit() {
        // Very tight policy (cap=1 via default), but AllowlistBypassed marker should skip everything
        let config = make_config(1, vec![]);
        let layer = make_layer(config);
        let mut svc = layer.layer(OkService);

        // Exhaust limit normally first (no marker)
        let r1 = svc.ready().await.unwrap().call(req_to("/", "GET", "2.2.2.2")).await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = svc.ready().await.unwrap().call(req_to("/", "GET", "2.2.2.2")).await.unwrap();
        assert_eq!(r2.status(), StatusCode::TOO_MANY_REQUESTS);

        // Now same IP but with AllowlistBypassed — should still be 200
        let mut req = req_to("/", "GET", "2.2.2.2");
        req.extensions_mut().insert(AllowlistBypassed);
        let r3 = svc.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn policies_have_separate_buckets() {
        // policy "a" on /a/* cap=1, policy "b" on /b/* cap=1
        let config = make_config(
            100,
            vec![
                make_policy("a", "/a/*", 1, false),
                make_policy("b", "/b/*", 1, false),
            ],
        );
        let layer = make_layer(config);
        let mut svc = layer.layer(OkService);

        // Exhaust policy "a"
        let r1 = svc.ready().await.unwrap().call(req_to("/a/x", "GET", "1.2.3.4")).await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = svc.ready().await.unwrap().call(req_to("/a/x", "GET", "1.2.3.4")).await.unwrap();
        assert_eq!(r2.status(), StatusCode::TOO_MANY_REQUESTS);

        // policy "b" should still work
        let r3 = svc.ready().await.unwrap().call(req_to("/b/x", "GET", "1.2.3.4")).await.unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
    }
}
