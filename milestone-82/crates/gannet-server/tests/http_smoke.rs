//! HTTP smoke tests exercising the full router (routes + middleware) without
//! binding a real socket.

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use gannet_server::{
    build_app,
    config::{ServerConfig, StorageSection},
    state::AppState,
};

fn test_state() -> AppState {
    let mut cfg = ServerConfig::default();
    cfg.storage = StorageSection::Memory;
    cfg.validate().expect("test config valid");
    AppState::new(cfg)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

#[tokio::test]
async fn health_returns_ok() {
    let app = build_app(test_state());
    let resp = app
        .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["storage_backend"], "memory");
    assert!(json["uptime_seconds"].is_u64());
}

#[tokio::test]
async fn liveness_and_readiness_probes_return_ok() {
    let app = build_app(test_state());

    for path in ["/v1/health/live", "/v1/health/ready"] {
        let resp = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "probe {path}");
        let json = body_json(resp).await;
        assert_eq!(json["status"], "ok", "probe {path}");
    }
}

#[tokio::test]
async fn root_returns_service_banner() {
    let app = build_app(test_state());
    let resp = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "gannet");
    assert_eq!(json["api_base"], "/v1");
}

#[tokio::test]
async fn metrics_exposes_build_info_and_request_counters() {
    let app = build_app(test_state());

    // Generate some traffic so the request counter has samples.
    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let resp = app
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("text/plain"));

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(text.contains("gannet_build_info"), "missing build info");
    assert!(
        text.contains("gannet_http_requests_total"),
        "missing request counter"
    );
    assert!(
        text.contains("path=\"/v1/health\""),
        "request counter must use matched-route labels"
    );
    assert!(
        text.contains("gannet_http_request_duration_seconds"),
        "missing latency histogram"
    );
}

#[tokio::test]
async fn unknown_route_returns_404_and_is_counted() {
    let app = build_app(test_state());

    let resp = app
        .clone()
        .oneshot(
            Request::get("/v1/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    // 404s are bucketed under a single label to bound cardinality.
    assert!(text.contains("path=\"<unmatched>\""));
    assert!(text.contains("status=\"404\""));
}
