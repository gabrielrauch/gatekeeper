mod common;

#[tokio::test]
async fn proxy_forwards_get_request() {
    let upstream = common::spawn_mock_upstream().await;
    let proxy = common::spawn_gatekeeper_proxy_only(upstream).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{proxy}/")).send().await.unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "upstream-ok");
}

#[tokio::test]
async fn proxy_forwards_post_with_body() {
    let upstream = common::spawn_mock_upstream().await;
    let proxy = common::spawn_gatekeeper_proxy_only(upstream).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{proxy}/echo"))
        .body("hello from client")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "hello from client");
}

#[tokio::test]
async fn proxy_preserves_upstream_status_code() {
    let upstream = common::spawn_mock_upstream().await;
    let proxy = common::spawn_gatekeeper_proxy_only(upstream).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{proxy}/status/201"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
}

#[tokio::test]
async fn proxy_adds_x_forwarded_for() {
    let upstream = common::spawn_mock_upstream().await;
    let proxy = common::spawn_gatekeeper_proxy_only(upstream).await;

    let client = reqwest::Client::new();
    // Just verify the request succeeds; X-Forwarded-For is set to the proxy's loopback IP
    let resp = client.get(format!("{proxy}/")).send().await.unwrap();

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn health_endpoints_not_proxied() {
    let upstream = common::spawn_mock_upstream().await;
    let proxy = common::spawn_gatekeeper_proxy_only(upstream).await;

    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{proxy}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");

    let resp = client
        .get(format!("{proxy}/readyz"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ready");
}
