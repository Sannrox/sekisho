//! HTTP host: `/healthz` and `/metrics`. Later endpoints authenticate
//! through [`AppState::authenticate`].

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;

use crate::metrics::Metrics;
use crate::verifier::{Principal, Rejection, Verifier};

pub struct AppState {
    pub verifier: Verifier,
    pub metrics: Metrics,
}

impl AppState {
    /// Verify the `Authorization: Bearer` header and count the outcome.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, Rejection> {
        let result = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Rejection::Malformed)
            .and_then(|token| self.verifier.verify(token));
        self.metrics.record(result.is_ok());
        result
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/metrics", get(metrics))
        .with_state(state)
}

async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(state.verifier.kind()),
    )
}
