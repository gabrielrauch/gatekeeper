use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use http::{Response, StatusCode};
use hyper::Request;
use tower::{Layer, Service};
use tracing::{error, warn};

use crate::algorithm::RateLimiter;
use crate::config::FailMode;
use crate::identity::IdentityExtractor;
use crate::response::build_429_response;

// ---------------------------------------------------------------------------
// RateLimitLayer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RateLimitLayer {
    limiter: Arc<dyn RateLimiter>,
    extractor: Arc<dyn IdentityExtractor>,
    fail_mode: FailMode,
    cost: u64,
}

impl RateLimitLayer {
    pub fn new(
        limiter: Arc<dyn RateLimiter>,
        extractor: Arc<dyn IdentityExtractor>,
        fail_mode: FailMode,
        cost: u64,
    ) -> Self {
        Self {
            limiter,
            extractor,
            fail_mode,
            cost,
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            limiter: self.limiter.clone(),
            extractor: self.extractor.clone(),
            fail_mode: self.fail_mode.clone(),
            cost: self.cost,
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimitService
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    limiter: Arc<dyn RateLimiter>,
    extractor: Arc<dyn IdentityExtractor>,
    fail_mode: FailMode,
    cost: u64,
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

        let limiter = self.limiter.clone();
        let extractor = self.extractor.clone();
        let fail_mode = self.fail_mode.clone();
        let cost = self.cost;

        Box::pin(async move {
            // 1. Extract identity key
            let identity_key = extractor
                .extract(&req)
                .map(|id| id.key)
                .unwrap_or_else(|| "unknown".to_string());

            // 2. Check rate limit, measure duration
            let check_start = std::time::Instant::now();
            let result = limiter.check(&identity_key, cost, false).await;
            let check_duration = check_start.elapsed();

            let algorithm = limiter.algorithm_name();
            metrics::histogram!(
                "gatekeeper_decision_duration_seconds",
                "algorithm" => algorithm,
            )
            .record(check_duration.as_secs_f64());

            match result {
                Ok(decision) if decision.allowed => {
                    // 3. Allowed
                    metrics::counter!(
                        "gatekeeper_requests_total",
                        "status" => "allowed",
                    )
                    .increment(1);

                    let mut resp = inner.call(req).await?;
                    resp.extensions_mut().insert(decision);
                    Ok(resp)
                }
                Ok(decision) => {
                    // 4. Denied
                    metrics::counter!(
                        "gatekeeper_requests_total",
                        "status" => "denied",
                    )
                    .increment(1);

                    Ok(build_429_response(&decision, "default"))
                }
                Err(e) => match fail_mode {
                    FailMode::Open => {
                        // 6. Error + open: warn and allow through
                        warn!("rate limiter error, failing open: {e}");
                        metrics::counter!(
                            "gatekeeper_requests_total",
                            "status" => "error",
                        )
                        .increment(1);
                        inner.call(req).await
                    }
                    FailMode::Closed => {
                        // 7. Error + closed: return 503
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
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use http::{Response, StatusCode};
    use hyper::Request;
    use tower::{Layer, Service, ServiceExt};

    use super::RateLimitLayer;
    use crate::algorithm::Decision;
    use crate::config::FailMode;
    use crate::identity::ip::IpExtractor;
    use crate::store::memory::MemoryStore;
    use crate::algorithm::token_bucket::TokenBucket;

    /// A trivially Clone + Send + 'static service that always returns 200.
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

    fn make_layer(capacity: u64) -> RateLimitLayer {
        let store = Arc::new(MemoryStore::new(1000));
        let limiter = Arc::new(TokenBucket::new(store, capacity, 0.0));
        let extractor = Arc::new(IpExtractor);
        RateLimitLayer::new(limiter, extractor, FailMode::Open, 1)
    }

    fn req_with_addr(ip: [u8; 4]) -> Request<Body> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])), 1234);
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn allows_request_within_limit() {
        let layer = make_layer(10);
        let mut svc = layer.layer(OkService);
        let req = req_with_addr([127, 0, 0, 1]);

        let resp = svc.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.extensions().get::<Decision>().is_some());
    }

    #[tokio::test]
    async fn returns_429_when_exhausted() {
        let layer = make_layer(2);
        let mut svc = layer.layer(OkService);

        let r1 = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_addr([10, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_addr([10, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::OK);

        let r3 = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_addr([10, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn different_ips_have_separate_buckets() {
        let layer = make_layer(1);
        let mut svc = layer.layer(OkService);

        let r1 = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_addr([10, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_addr([10, 0, 0, 2]))
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
    }
}
