use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use pensyve_mcp_gateway::admission::{
    MIB, RecallAdmission, enforce_recall_admission, recall_overload_count,
};
use tower::ServiceExt;

#[tokio::test]
async fn admission_caps_permits_and_reserved_bytes() {
    let admission = RecallAdmission::new(8, 64 * MIB);
    let mut reservations = Vec::new();
    for _ in 0..8 {
        reservations.push(admission.acquire(8 * MIB).await.unwrap());
    }

    assert!(admission.try_acquire(8 * MIB).is_err());
    assert_eq!(admission.reserved_bytes(), 64 * MIB);
    drop(reservations);
    assert_eq!(admission.reserved_bytes(), 0);
}

#[tokio::test]
async fn cancellation_releases_the_raii_reservation() {
    let admission = Arc::new(RecallAdmission::new(1, 8 * MIB));
    let task_admission = Arc::clone(&admission);
    let task = tokio::spawn(async move {
        let _reservation = task_admission.acquire(8 * MIB).await.unwrap();
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    assert_eq!(admission.reserved_bytes(), 8 * MIB);

    task.abort();
    let _ = task.await;
    assert_eq!(admission.reserved_bytes(), 0);
    assert!(admission.try_acquire(8 * MIB).is_ok());
}

#[tokio::test]
async fn overloaded_http_recall_returns_retry_after_before_handler_work() {
    let admission = Arc::new(RecallAdmission::new(1, 8 * MIB));
    let held = admission.acquire(8 * MIB).await.unwrap();
    let overloads_before = recall_overload_count();
    let handler_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&handler_calls);
    let app = Router::new()
        .route(
            "/v1/recall",
            post(move || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&admission),
            enforce_recall_admission,
        ));

    let response = app
        .oneshot(Request::post("/v1/recall").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.overload_count(), 1);
    assert!(recall_overload_count() > overloads_before);
    drop(held);
}
