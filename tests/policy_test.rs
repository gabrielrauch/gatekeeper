mod common;

use gatekeeper::config::{AccessConfig, MatchRule, PolicyConfig};

fn make_policy(
    name: &str,
    path: &str,
    capacity: Option<u64>,
    bypass: Option<bool>,
) -> PolicyConfig {
    PolicyConfig {
        name: name.to_string(),
        match_rule: MatchRule {
            path: path.to_string(),
            methods: None,
        },
        algorithm: None,
        capacity,
        refill_rate: Some(0.0),
        cost: Some(1),
        identify_by: None,
        bypass,
    }
}

#[tokio::test]
async fn policy_applies_different_limits_by_path() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper_with_policies(
        upstream,
        100,   // default capacity
        0.01,
        vec![make_policy("reports", "/api/reports/*", Some(2), None)],
        AccessConfig::default(),
    )
    .await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // /api/reports/* is limited to 2
    let r1 = client.get(format!("{base}/api/reports/q1")).send().await.unwrap();
    assert_eq!(r1.status(), 200, "1st reports request should be allowed");

    let r2 = client.get(format!("{base}/api/reports/q1")).send().await.unwrap();
    assert_eq!(r2.status(), 200, "2nd reports request should be allowed");

    let r3 = client.get(format!("{base}/api/reports/q1")).send().await.unwrap();
    assert_eq!(r3.status(), 429, "3rd reports request should be rate-limited");

    // default path still works (capacity=100)
    let r4 = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(r4.status(), 200, "default path should still be allowed");
}

#[tokio::test]
async fn bypass_policy_skips_rate_limiting() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper_with_policies(
        upstream,
        1,   // very tight default
        0.01,
        vec![make_policy("webhooks", "/webhooks/*", None, Some(true))],
        AccessConfig::default(),
    )
    .await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // Many requests to /webhooks/* — all should be 200 (bypass)
    for i in 1..=5 {
        let r = client
            .post(format!("{base}/webhooks/event"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "webhooks request {i} should bypass rate limit");
    }
}

#[tokio::test]
async fn denylist_blocks_ip() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper_with_policies(
        upstream,
        100,
        10.0,
        vec![],
        AccessConfig {
            denylist_ips: vec!["127.0.0.1/32".to_string()],
            ..Default::default()
        },
    )
    .await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        403,
        "127.0.0.1 is in denylist, should get 403"
    );
}

#[tokio::test]
async fn denylist_blocks_api_key() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper_with_policies(
        upstream,
        100,
        10.0,
        vec![],
        AccessConfig {
            denylist_keys: vec!["sk-bad".to_string()],
            ..Default::default()
        },
    )
    .await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // With bad key → 403
    let resp = client
        .get(format!("{base}/"))
        .header("x-api-key", "sk-bad")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "sk-bad key should be denied");

    // Without key → 200
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "request without key should be allowed");
}

#[tokio::test]
async fn allowlist_bypasses_rate_limit() {
    let upstream = common::spawn_mock_upstream().await;
    let addr = common::spawn_gatekeeper_with_policies(
        upstream,
        1,   // extremely tight: 1 request per identity
        0.001,
        vec![],
        AccessConfig {
            allowlist_ips: vec!["127.0.0.0/8".to_string()],
            ..Default::default()
        },
    )
    .await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();

    // 127.0.0.1 is in the allowlist (127.0.0.0/8), so all requests should be 200
    for i in 1..=5 {
        let resp = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(
            resp.status(),
            200,
            "allowlisted request {i} should bypass rate limit"
        );
    }
}
