use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub use pensyve_mcp_tools::{
    MIB, RecallAdmission, RecallOverloaded, RecallReservation, recall_overload_count,
};

const RECALL_RESERVATION_BYTES: usize = 8 * MIB;

/// Reserve gateway recall capacity before any handler can resolve entities,
/// embed the query, or hydrate candidates. The guard spans the downstream
/// future, so disconnect/cancellation releases it through `Drop`.
pub async fn enforce_recall_admission(
    State(admission): State<Arc<RecallAdmission>>,
    request: Request,
    next: Next,
) -> Response {
    if !is_recall_path(request.uri().path()) {
        return next.run(request).await;
    }
    let Ok(_reservation) = admission.try_acquire(RECALL_RESERVATION_BYTES) else {
        tracing::warn!(
            event = "recall_overload",
            surface = "http",
            reserved_bytes = admission.reserved_bytes(),
            "recall_overload"
        );
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "recall overloaded",
                "retryable": true
            })),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    next.run(request).await
}

fn is_recall_path(path: &str) -> bool {
    matches!(path, "/v1/recall" | "/v1/recall_grouped" | "/v1/a2a/task")
}
