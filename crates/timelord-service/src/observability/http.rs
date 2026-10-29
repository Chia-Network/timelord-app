//! Read-only HTTP projection of cached operational state.
use super::{Metrics, snapshot};
use crate::Shared;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone)]
pub struct HttpState {
    pub current: watch::Receiver<Arc<Shared>>,
    pub metrics: Arc<Metrics>,
    pub process: Arc<crate::operational::State>,
}
pub fn routes() -> Router<HttpState> {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}
async fn metrics(State(state): State<HttpState>) -> impl IntoResponse {
    let shared = state.current.borrow().clone();
    match state.metrics.render(&shared, crate::unix_now()) {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        ),
        Err(error) => {
            tracing::error!(%error,"Metrics encoding failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Metrics unavailable\n".into(),
            )
        }
    }
}
async fn healthz(State(state): State<HttpState>) -> impl IntoResponse {
    if state.process.snapshot().healthy() {
        (StatusCode::OK, "Ok\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "Unavailable\n")
    }
}
async fn readyz(State(state): State<HttpState>) -> impl IntoResponse {
    let shared = state.current.borrow().clone();
    let (_, ready, _) = snapshot(&shared, crate::unix_now());
    (
        if ready.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(ready),
    )
}
