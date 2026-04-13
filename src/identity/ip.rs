use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::Request;

use super::{Identity, IdentityExtractor, IdentityKind};

pub struct IpExtractor;

impl IdentityExtractor for IpExtractor {
    fn extract(&self, req: &Request<Body>) -> Option<Identity> {
        // Try X-Forwarded-For first
        if let Some(xff) = req.headers().get("x-forwarded-for") {
            if let Ok(val) = xff.to_str() {
                let first = val.split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    return Some(Identity {
                        key: format!("ip:{first}"),
                        kind: IdentityKind::Ip,
                    });
                }
            }
        }

        // Try X-Real-IP
        if let Some(xri) = req.headers().get("x-real-ip") {
            if let Ok(val) = xri.to_str() {
                let ip = val.trim();
                if !ip.is_empty() {
                    return Some(Identity {
                        key: format!("ip:{ip}"),
                        kind: IdentityKind::Ip,
                    });
                }
            }
        }

        // Fall back to ConnectInfo
        if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
            return Some(Identity {
                key: format!("ip:{}", addr.ip()),
                kind: IdentityKind::Ip,
            });
        }

        None
    }

    fn extractor_name(&self) -> &'static str {
        "ip"
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use hyper::Request;

    use super::IpExtractor;
    use crate::identity::IdentityExtractor;

    fn empty_req() -> Request<Body> {
        Request::builder().uri("/").body(Body::empty()).unwrap()
    }

    #[test]
    fn extracts_from_x_forwarded_for() {
        let mut req = empty_req();
        req.headers_mut()
            .insert("x-forwarded-for", "203.0.113.50, 10.0.0.1".parse().unwrap());

        let identity = IpExtractor.extract(&req).unwrap();
        assert_eq!(identity.key, "ip:203.0.113.50");
    }

    #[test]
    fn extracts_from_x_real_ip() {
        let mut req = empty_req();
        req.headers_mut()
            .insert("x-real-ip", "198.51.100.10".parse().unwrap());

        let identity = IpExtractor.extract(&req).unwrap();
        assert_eq!(identity.key, "ip:198.51.100.10");
    }

    #[test]
    fn x_forwarded_for_takes_precedence() {
        let mut req = empty_req();
        req.headers_mut()
            .insert("x-forwarded-for", "203.0.113.50".parse().unwrap());
        req.headers_mut()
            .insert("x-real-ip", "198.51.100.10".parse().unwrap());

        let identity = IpExtractor.extract(&req).unwrap();
        assert_eq!(identity.key, "ip:203.0.113.50");
    }

    #[test]
    fn falls_back_to_connect_info() {
        let mut req = empty_req();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 1234);
        req.extensions_mut().insert(ConnectInfo(addr));

        let identity = IpExtractor.extract(&req).unwrap();
        assert_eq!(identity.key, "ip:192.168.1.1");
    }

    #[test]
    fn returns_none_without_any_source() {
        let req = empty_req();
        assert!(IpExtractor.extract(&req).is_none());
    }
}
