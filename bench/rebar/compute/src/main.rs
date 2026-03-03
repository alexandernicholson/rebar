use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::{get, post}};
use bench_common::{ComputeRequest, ComputeResponse, COMPUTE_HTTP_PORT, fib};
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::{RestartStrategy, SupervisorSpec, start_supervisor};
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
}

async fn health() -> &'static str {
    "ok"
}

async fn compute(
    State(state): State<AppState>,
    Json(req): Json<ComputeRequest>,
) -> Result<Json<ComputeResponse>, axum::http::StatusCode> {
    if req.n < 0 || req.n > 92 {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    let n = req.n as u64;
    let (tx, rx) = tokio::sync::oneshot::channel();

    state.runtime.spawn(move |_ctx| async move {
        let result = fib(n);
        let _ = tx.send(result);
    }).await;

    match rx.await {
        Ok(result) => Ok(Json(ComputeResponse { result })),
        Err(_) => Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let runtime = Arc::new(Runtime::new(1));

    // Start a supervisor for the compute service
    let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
        .max_restarts(100)
        .max_seconds(60);
    let _supervisor = start_supervisor(runtime.clone(), spec, vec![]).await;

    let state = AppState { runtime };

    let app = Router::new()
        .route("/health", get(health))
        .route("/compute", post(compute))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", COMPUTE_HTTP_PORT);
    tracing::info!("Compute service listening on {}", addr);
    let listener = TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
