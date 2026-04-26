//! End-to-end fetch tests using httpmock.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use anie_tools_web::WebToolError;
use anie_tools_web::read::fetch::{FetchOptions, build_client, fetch_html};
use httpmock::Method::GET;
use httpmock::MockServer;
use url::Url;

fn opts_for_test(allow_private: bool) -> FetchOptions {
    FetchOptions {
        allow_private_ips: allow_private,
        timeout: std::time::Duration::from_secs(5),
        ..FetchOptions::default()
    }
}

#[tokio::test]
async fn fetch_returns_body_within_max_size() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/article");
            then.status(200)
                .header("content-type", "text/html; charset=utf-8")
                .body("<html><body><h1>Hello</h1></body></html>");
        })
        .await;

    let url = Url::parse(&format!("{}/article", server.base_url())).unwrap();
    let opts = opts_for_test(true);
    let client = build_client(&opts).expect("build client");
    let html = fetch_html(&client, &url, &opts).await.expect("fetch ok");
    assert!(html.contains("<h1>Hello</h1>"));
}

#[tokio::test]
async fn fetch_rejects_body_above_max_size() {
    let server = MockServer::start_async().await;
    let big = "x".repeat(50 * 1024); // 50 KiB body
    server
        .mock_async(|when, then| {
            when.method(GET).path("/big");
            then.status(200).body(big);
        })
        .await;

    let url = Url::parse(&format!("{}/big", server.base_url())).unwrap();
    let mut opts = opts_for_test(true);
    opts.max_bytes = 10 * 1024; // 10 KiB cap
    let client = build_client(&opts).expect("build client");
    let err = fetch_html(&client, &url, &opts).await.unwrap_err();
    assert!(matches!(err, WebToolError::TooLarge { .. }), "got: {err:?}");
}

#[tokio::test]
async fn fetch_surfaces_http_404_as_typed_error() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/missing");
            then.status(404).body("page not found");
        })
        .await;

    let url = Url::parse(&format!("{}/missing", server.base_url())).unwrap();
    let opts = opts_for_test(true);
    let client = build_client(&opts).expect("build client");
    let err = fetch_html(&client, &url, &opts).await.unwrap_err();
    match err {
        WebToolError::HttpStatus { code, body_excerpt } => {
            assert_eq!(code, 404);
            assert!(body_excerpt.contains("not found"));
        }
        other => panic!("expected HttpStatus, got: {other:?}"),
    }
}

#[tokio::test]
async fn fetch_follows_redirect_chain() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/start");
            then.status(302)
                .header("Location", "/middle");
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/middle");
            then.status(302)
                .header("Location", "/final");
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/final");
            then.status(200).body("<html>arrived</html>");
        })
        .await;

    let url = Url::parse(&format!("{}/start", server.base_url())).unwrap();
    let opts = opts_for_test(true);
    let client = build_client(&opts).expect("build client");
    let html = fetch_html(&client, &url, &opts).await.expect("fetch ok");
    assert!(html.contains("arrived"));
}

#[tokio::test]
async fn fetch_caps_redirect_chain() {
    let server = MockServer::start_async().await;
    // Build a 12-hop chain. Cap is 10 by default.
    for i in 0..12 {
        let next = if i == 11 {
            "/final".to_string()
        } else {
            format!("/hop{}", i + 1)
        };
        server
            .mock_async(move |when, then| {
                let path = if i == 0 {
                    "/start".to_string()
                } else {
                    format!("/hop{i}")
                };
                when.method(GET).path(path);
                then.status(302).header("Location", next);
            })
            .await;
    }

    let url = Url::parse(&format!("{}/start", server.base_url())).unwrap();
    let mut opts = opts_for_test(true);
    opts.max_redirects = 5;
    let client = build_client(&opts).expect("build client");
    let err = fetch_html(&client, &url, &opts).await.unwrap_err();
    assert!(matches!(err, WebToolError::Fetch(_)), "got: {err:?}");
}
