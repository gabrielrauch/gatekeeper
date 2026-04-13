use axum::body::Body;
use hyper::{Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use http_body_util::BodyExt;

use super::{ProxyError, ProxyHandler};

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub struct HttpProxy {
    upstream_url: String,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
}

impl HttpProxy {
    pub fn new(upstream_url: String) -> Self {
        let client = Client::builder(TokioExecutor::new()).build_http();
        Self {
            upstream_url,
            client,
        }
    }
}

#[async_trait::async_trait]
impl ProxyHandler for HttpProxy {
    async fn proxy(
        &self,
        req: Request<Body>,
        client_ip: Option<std::net::IpAddr>,
    ) -> Result<Response<Body>, ProxyError> {
        let upstream_base = self
            .upstream_url
            .trim_end_matches('/')
            .to_string();

        // Build the new URI: upstream base + original path+query
        let original_uri = req.uri();
        let path_and_query = original_uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");

        let new_uri_str = format!("{}{}", upstream_base, path_and_query);
        let new_uri: hyper::Uri = new_uri_str.parse().map_err(|e| {
            ProxyError::InvalidUri(format!("{new_uri_str}: {e}"))
        })?;

        // Parse upstream authority for the Host header
        let upstream_authority = new_uri.authority().map(|a| a.to_string());

        // Build the outgoing request
        let (mut parts, body) = req.into_parts();

        // Strip hop-by-hop headers from request
        for header in HOP_BY_HOP_HEADERS {
            parts.headers.remove(*header);
        }

        // Add / append X-Forwarded-For
        if let Some(ip) = client_ip {
            let ip_str = ip.to_string();
            let new_value = if let Some(existing) = parts.headers.get("x-forwarded-for") {
                let existing_str = existing.to_str().unwrap_or("");
                format!("{existing_str}, {ip_str}")
            } else {
                ip_str
            };
            parts.headers.insert(
                hyper::header::HeaderName::from_static("x-forwarded-for"),
                new_value.parse().map_err(|e| {
                    ProxyError::Request(format!("invalid X-Forwarded-For value: {e}"))
                })?,
            );
        }

        // Set Host to upstream authority
        if let Some(authority) = upstream_authority {
            parts.headers.insert(
                hyper::header::HOST,
                authority.parse().map_err(|e| {
                    ProxyError::Request(format!("invalid Host header: {e}"))
                })?,
            );
        }

        parts.uri = new_uri;

        let outgoing_req = Request::from_parts(parts, body);

        // Send to upstream
        let upstream_response = self
            .client
            .request(outgoing_req)
            .await
            .map_err(|e| ProxyError::Request(e.to_string()))?;

        // Strip hop-by-hop headers from response and convert Incoming body to Body
        let (mut resp_parts, resp_body) = upstream_response.into_parts();

        for header in HOP_BY_HOP_HEADERS {
            resp_parts.headers.remove(*header);
        }

        let body = Body::new(resp_body.map_err(|e| {
            Box::new(e) as Box<dyn std::error::Error + Send + Sync>
        }));

        Ok(Response::from_parts(resp_parts, body))
    }

    fn protocol_name(&self) -> &'static str {
        "http"
    }
}
