mod common;

#[tokio::test]
async fn rate_limits_by_ip() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper(upstream, 3, 0.01).await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // First 3 requests should be allowed
    for i in 1..=3 {
        let resp = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(resp.status(), 200, "request {i} should be allowed");
    }

    // 4th request should be rate-limited
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), 429, "4th request should be rate-limited");

    let body = resp.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["error"], "rate_limit_exceeded",
        "body should contain error=rate_limit_exceeded, got: {body}"
    );
}

#[tokio::test]
async fn response_includes_rate_limit_headers() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper(upstream, 100, 10.0).await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base}/")).send().await.unwrap();

    assert_eq!(resp.status(), 200);

    let limit = resp
        .headers()
        .get("x-ratelimit-limit")
        .expect("x-ratelimit-limit header missing")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(limit, 100, "x-ratelimit-limit should be 100");

    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .expect("x-ratelimit-remaining header missing")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(remaining, 99, "x-ratelimit-remaining should be 99");

    assert!(
        resp.headers().get("x-ratelimit-reset").is_some(),
        "x-ratelimit-reset header missing"
    );
}

#[tokio::test]
async fn denied_response_includes_retry_after() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper(upstream, 1, 0.01).await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // Exhaust the bucket
    let first = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(first.status(), 200, "first request should succeed");

    // Now it should be denied
    let denied = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(denied.status(), 429, "second request should be rate-limited");

    assert!(
        denied.headers().get("retry-after").is_some(),
        "retry-after header missing on 429 response"
    );
}

#[tokio::test]
async fn health_endpoints_bypass_rate_limit() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper(upstream, 1, 0.01).await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // Exhaust the rate limit with a proxy request
    let first = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let limited = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(limited.status(), 429, "proxy route should be rate-limited");

    // Health endpoint should bypass rate limiting
    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(
        health.status(),
        200,
        "/healthz should bypass rate limiting"
    );
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_format() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper(upstream, 100, 10.0).await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // Make a request to generate some metrics
    client.get(format!("{base}/")).send().await.unwrap();

    // Fetch metrics
    let resp = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "/metrics should return 200");

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("gatekeeper_"),
        "/metrics body should contain 'gatekeeper_' metrics, got: {body}"
    );
}
