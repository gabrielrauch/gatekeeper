pub mod http;

use axum::body::Body;
use hyper::{Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("upstream request failed: {0}")]
    Request(String),
    #[error("invalid upstream URI: {0}")]
    InvalidUri(String),
}

#[async_trait::async_trait]
pub trait ProxyHandler: Send + Sync + 'static {
    async fn proxy(
        &self,
        req: Request<Body>,
        client_ip: Option<std::net::IpAddr>,
    ) -> Result<Response<Body>, ProxyError>;
    fn protocol_name(&self) -> &'static str;
}
