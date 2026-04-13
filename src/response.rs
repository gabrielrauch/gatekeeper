use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use http::{Response, StatusCode};
use hyper::Request;
use tower::{Layer, Service};

use crate::algorithm::Decision;

// ---------------------------------------------------------------------------
// build_429_response
// ---------------------------------------------------------------------------

pub fn build_429_response(decision: &Decision, policy_name: &str) -> Response<Body> {
    let retry_after_secs = decision
        .retry_after
        .unwrap_or(Duration::from_secs(1))
        .as_secs()
        .max(1);

    let body = serde_json::json!({
        "error": "rate_limit_exceeded",
        "message": format!("Too many requests. Retry after {} seconds.", retry_after_secs),
        "retry_after": retry_after_secs,
        "limit": decision.limit,
        "policy": policy_name,
    });

    let json_bytes = body.to_string();

    let mut resp = Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "application/json")
        .header("retry-after", retry_after_secs.to_string())
        .body(Body::from(json_bytes))
        .expect("build_429_response: builder should not fail");

    resp.extensions_mut().insert(decision.clone());
    resp
}

// ---------------------------------------------------------------------------
// inject_rate_limit_headers helper
// ---------------------------------------------------------------------------

pub fn inject_rate_limit_headers(resp: &mut Response<Body>) {
    let decision = match resp.extensions().get::<Decision>() {
        Some(d) => d.clone(),
        None => return,
    };

    let headers = resp.headers_mut();

    headers.insert(
        "x-ratelimit-limit",
        decision.limit.to_string().parse().unwrap(),
    );
    headers.insert(
        "x-ratelimit-remaining",
        decision.remaining.to_string().parse().unwrap(),
    );

    // Convert reset_at (Instant) to Unix timestamp
    let duration_until_reset = decision.reset_at.saturating_duration_since(Instant::now());
    let reset_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        + duration_until_reset;
    headers.insert(
        "x-ratelimit-reset",
        reset_unix.as_secs().to_string().parse().unwrap(),
    );

    if let Some(retry_after) = decision.retry_after {
        headers.insert(
            "retry-after",
            retry_after.as_secs().max(1).to_string().parse().unwrap(),
        );
    }
}

// ---------------------------------------------------------------------------
// ResponseHeaderLayer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ResponseHeaderLayer;

impl<S> Layer<S> for ResponseHeaderLayer {
    type Service = ResponseHeaderService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ResponseHeaderService { inner }
    }
}

// ---------------------------------------------------------------------------
// ResponseHeaderService
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ResponseHeaderService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for ResponseHeaderService<S>
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
        Box::pin(async move {
            let mut resp = inner.call(req).await?;
            inject_rate_limit_headers(&mut resp);
            Ok(resp)
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::{Duration, Instant};

    use axum::body::Body;
    use http::{Response, StatusCode};
    use hyper::Request;
    use tower::{Layer, Service, ServiceExt, service_fn};

    use super::{ResponseHeaderLayer, build_429_response};
    use crate::algorithm::Decision;

    fn make_decision() -> Decision {
        Decision {
            allowed: false,
            remaining: 0,
            limit: 10,
            reset_at: Instant::now() + Duration::from_secs(5),
            retry_after: Some(Duration::from_secs(3)),
        }
    }

    #[test]
    fn build_429_has_correct_status() {
        let d = make_decision();
        let resp = build_429_response(&d, "test");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get("retry-after").is_some());
    }

    #[test]
    fn build_429_has_decision_in_extensions() {
        let d = make_decision();
        let resp = build_429_response(&d, "test");
        assert!(resp.extensions().get::<Decision>().is_some());
    }

    #[tokio::test]
    async fn header_layer_injects_on_response_with_decision() {
        let d = Decision {
            allowed: true,
            remaining: 7,
            limit: 10,
            reset_at: Instant::now() + Duration::from_secs(10),
            retry_after: None,
        };

        let d_clone = d.clone();
        let svc = service_fn(move |_req: Request<Body>| {
            let d = d_clone.clone();
            async move {
                let mut resp = Response::new(Body::empty());
                resp.extensions_mut().insert(d);
                Ok::<_, Infallible>(resp)
            }
        });

        let mut layered = ResponseHeaderLayer.layer(svc);
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = layered.ready().await.unwrap().call(req).await.unwrap();

        assert!(resp.headers().get("x-ratelimit-limit").is_some());
        assert!(resp.headers().get("x-ratelimit-remaining").is_some());
        assert!(resp.headers().get("x-ratelimit-reset").is_some());
        assert_eq!(
            resp.headers()
                .get("x-ratelimit-limit")
                .unwrap()
                .to_str()
                .unwrap(),
            "10"
        );
        assert_eq!(
            resp.headers()
                .get("x-ratelimit-remaining")
                .unwrap()
                .to_str()
                .unwrap(),
            "7"
        );
    }

    #[tokio::test]
    async fn header_layer_no_op_without_decision() {
        let svc = service_fn(|_req: Request<Body>| async {
            Ok::<_, Infallible>(Response::new(Body::empty()))
        });

        let mut layered = ResponseHeaderLayer.layer(svc);
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = layered.ready().await.unwrap().call(req).await.unwrap();

        assert!(resp.headers().get("x-ratelimit-limit").is_none());
        assert!(resp.headers().get("x-ratelimit-remaining").is_none());
        assert!(resp.headers().get("x-ratelimit-reset").is_none());
    }
}
