use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::ConnectInfo;
use http::{Response, StatusCode};
use hyper::Request;
use ipnet::IpNet;
use tower::{Layer, Service};

use crate::config::AccessConfig;

// ---------------------------------------------------------------------------
// AllowlistBypassed marker
// ---------------------------------------------------------------------------

/// Inserted into request extensions when a request is allowlisted.
/// The RateLimitLayer checks for this and skips rate limiting.
#[derive(Debug, Clone)]
pub struct AllowlistBypassed;

// ---------------------------------------------------------------------------
// AccessControl
// ---------------------------------------------------------------------------

pub struct AccessControl {
    denylist_ips: Vec<IpNet>,
    denylist_keys: HashSet<String>,
    allowlist_ips: Vec<IpNet>,
    allowlist_keys: HashSet<String>,
}

impl AccessControl {
    pub fn from_config(config: &AccessConfig) -> Self {
        let denylist_ips = config
            .denylist_ips
            .iter()
            .filter_map(|s| s.parse::<IpNet>().ok())
            .collect();
        let allowlist_ips = config
            .allowlist_ips
            .iter()
            .filter_map(|s| s.parse::<IpNet>().ok())
            .collect();
        let denylist_keys = config.denylist_keys.iter().cloned().collect();
        let allowlist_keys = config.allowlist_keys.iter().cloned().collect();

        Self {
            denylist_ips,
            denylist_keys,
            allowlist_ips,
            allowlist_keys,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.denylist_ips.is_empty()
            && self.denylist_keys.is_empty()
            && self.allowlist_ips.is_empty()
            && self.allowlist_keys.is_empty()
    }

    pub fn is_denied(&self, ip: Option<IpAddr>, api_key: Option<&str>) -> bool {
        if let Some(key) = api_key {
            if self.denylist_keys.contains(key) {
                return true;
            }
        }
        if let Some(addr) = ip {
            for net in &self.denylist_ips {
                if net.contains(&addr) {
                    return true;
                }
            }
        }
        false
    }

    pub fn is_allowed(&self, ip: Option<IpAddr>, api_key: Option<&str>) -> bool {
        if let Some(key) = api_key {
            if self.allowlist_keys.contains(key) {
                return true;
            }
        }
        if let Some(addr) = ip {
            for net in &self.allowlist_ips {
                if net.contains(&addr) {
                    return true;
                }
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// AccessControlLayer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AccessControlLayer {
    access_control: std::sync::Arc<AccessControl>,
}

impl AccessControlLayer {
    pub fn new(access_control: AccessControl) -> Self {
        Self {
            access_control: std::sync::Arc::new(access_control),
        }
    }
}

impl<S> Layer<S> for AccessControlLayer {
    type Service = AccessControlService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessControlService {
            inner,
            access_control: self.access_control.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// AccessControlService
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AccessControlService<S> {
    inner: S,
    access_control: std::sync::Arc<AccessControl>,
}

fn build_403_response() -> Response<Body> {
    let body = serde_json::json!({
        "error": "forbidden",
        "message": "Access denied",
    });
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build_403_response: builder should not fail")
}

impl<S> Service<Request<Body>> for AccessControlService<S>
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

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        let ac = self.access_control.clone();

        Box::pin(async move {
            // 1. Passthrough when no rules configured
            if ac.is_empty() {
                return inner.call(req).await;
            }

            // 2. Extract client IP from ConnectInfo
            let ip = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());

            // 3. Extract API key from x-api-key header
            let api_key = req
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let api_key_ref = api_key.as_deref();

            // 4. Denylist check (takes precedence)
            if ac.is_denied(ip, api_key_ref) {
                return Ok(build_403_response());
            }

            // 5. Allowlist check
            if ac.is_allowed(ip, api_key_ref) {
                req.extensions_mut().insert(AllowlistBypassed);
            }

            // 6. Forward (with or without bypass marker)
            inner.call(req).await
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
    use std::task::{Context, Poll};

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use http::{Response, StatusCode};
    use hyper::Request;
    use tower::{Layer, Service, ServiceExt};

    use super::{AccessControl, AccessControlLayer, AllowlistBypassed};
    use crate::config::AccessConfig;

    #[derive(Clone)]
    struct OkService;

    impl Service<Request<Body>> for OkService {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            // Echo back the request extensions so tests can inspect them
            Box::pin(async move {
                let mut resp = Response::new(Body::empty());
                if req.extensions().get::<AllowlistBypassed>().is_some() {
                    resp.extensions_mut().insert(AllowlistBypassed);
                }
                Ok(resp)
            })
        }
    }

    fn req_with_ip(ip_str: &str) -> Request<Body> {
        let ip: IpAddr = ip_str.parse().unwrap();
        let addr = SocketAddr::new(ip, 12345);
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn req_with_ip_and_key(ip_str: &str, key: &str) -> Request<Body> {
        let ip: IpAddr = ip_str.parse().unwrap();
        let addr = SocketAddr::new(ip, 12345);
        let mut req = Request::builder()
            .uri("/")
            .header("x-api-key", key)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn make_layer(config: AccessConfig) -> AccessControlLayer {
        let ac = AccessControl::from_config(&config);
        AccessControlLayer::new(ac)
    }

    #[tokio::test]
    async fn denies_by_ip() {
        let layer = make_layer(AccessConfig {
            denylist_ips: vec!["192.168.1.1/32".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip("192.168.1.1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn denies_by_cidr() {
        let layer = make_layer(AccessConfig {
            denylist_ips: vec!["10.0.0.0/8".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip("10.5.6.7"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn denies_by_api_key() {
        let layer = make_layer(AccessConfig {
            denylist_keys: vec!["bad-key".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip_and_key("1.2.3.4", "bad-key"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allows_by_ip_inserts_bypass_marker() {
        let layer = make_layer(AccessConfig {
            allowlist_ips: vec!["10.0.0.0/8".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip("10.1.2.3"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.extensions().get::<AllowlistBypassed>().is_some(),
            "AllowlistBypassed marker should be present"
        );
    }

    #[tokio::test]
    async fn allows_by_api_key() {
        let layer = make_layer(AccessConfig {
            allowlist_keys: vec!["good-key".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip_and_key("5.5.5.5", "good-key"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.extensions().get::<AllowlistBypassed>().is_some());
    }

    #[tokio::test]
    async fn passes_through_when_not_listed() {
        let layer = make_layer(AccessConfig {
            denylist_ips: vec!["1.2.3.4/32".to_string()],
            allowlist_ips: vec!["10.0.0.0/8".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        // IP that is neither denied nor allowed
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip("5.5.5.5"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.extensions().get::<AllowlistBypassed>().is_none(),
            "No bypass marker when not in allowlist"
        );
    }

    #[tokio::test]
    async fn denylist_takes_precedence_over_allowlist() {
        // IP is in both deny and allow
        let layer = make_layer(AccessConfig {
            denylist_ips: vec!["10.0.0.0/8".to_string()],
            allowlist_ips: vec!["10.0.0.0/8".to_string()],
            ..Default::default()
        });
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip("10.1.2.3"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn empty_access_control_passes_through() {
        let layer = make_layer(AccessConfig::default());
        let mut svc = layer.layer(OkService);
        let resp = svc
            .ready()
            .await
            .unwrap()
            .call(req_with_ip("1.2.3.4"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
