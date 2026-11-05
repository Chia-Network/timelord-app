//! Private sockets and fixture responses only; no launchd registration or USB.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use tower::ServiceExt;
#[tokio::test]
async fn response_budget_is_path_independent_and_held_until_body_drop() {
    let app = timelord_supervisor::http::protect(
        Router::new().route("/fixture/stream", get(|| async { "fixture history" })),
        4,
    );
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/fixture/stream")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        );
    }
    let busy = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/fixture/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(held.pop());
    let ready = app
        .oneshot(
            Request::builder()
                .uri("/fixture/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    assert_eq!(ready.headers()["content-length"], "15");
    assert_eq!(
        axum::body::to_bytes(ready.into_body(), 1024).await.unwrap(),
        "fixture history"
    );
}
