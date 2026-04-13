pub mod ip;

use axum::body::Body;
use hyper::Request;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct Identity {
    pub key: String,
    pub kind: IdentityKind,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum IdentityKind {
    Ip,
}

pub trait IdentityExtractor: Send + Sync + 'static {
    fn extract(&self, req: &Request<Body>) -> Option<Identity>;
    fn extractor_name(&self) -> &'static str;
}
