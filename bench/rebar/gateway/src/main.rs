use std::time::Duration;

use axum::{Json, Router, extract::{Path, State}, http::StatusCode, routing::{get, post}};
use bench_common::{
    COMPUTE_HTTP_PORT, GATEWAY_HTTP_PORT, STORE_HTTP_PORT,
    ComputeRequest, ComputeResponse, StoreResponse, StoreValue,
};
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    compute_base: String,
    store_base: String,
}

async fn health(State(state): State<AppState>) -> Result<&'static str, StatusCode> {
    let compute_health = state
        .client
        .get(format!("{}/health", state.compute_base))
        .send()
        .await;
    let store_health = state
        .client
        .get(format!("{}/health", state.store_base))
        .send()
        .await;

    match (compute_health, store_health) {
        (Ok(c), Ok(s)) if c.status().is_success() && s.status().is_success() => Ok("ok"),
        _ => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn compute(
    State(state): State<AppState>,
    Json(req): Json<ComputeRequest>,
) -> Result<Json<ComputeResponse>, StatusCode> {
    let resp = state
        .client
        .post(format!("{}/compute", state.compute_base))
        .json(&req)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if !resp.status().is_success() {
        return Err(StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    }

    let body: ComputeResponse = resp.json().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(Json(body))
}

async fn store_get(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<StoreResponse>, StatusCode> {
    let resp = state
        .client
        .get(format!("{}/store/{}", state.store_base, key))
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(StatusCode::NOT_FOUND);
    }
    if !resp.status().is_success() {
        return Err(StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    }

    let body: StoreResponse = resp.json().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(Json(body))
}

async fn store_put(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<StoreValue>,
) -> Result<StatusCode, StatusCode> {
    let resp = state
        .client
        .put(format!("{}/store/{}", state.store_base, key))
        .json(&body)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if !resp.status().is_success() {
        return Err(StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    }

    Ok(StatusCode::OK)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let compute_host = std::env::var("COMPUTE_HOST").unwrap_or_else(|_| "compute".to_string());
    let store_host = std::env::var("STORE_HOST").unwrap_or_else(|_| "store".to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let state = AppState {
        client,
        compute_base: format!("http://{}:{}", compute_host, COMPUTE_HTTP_PORT),
        store_base: format!("http://{}:{}", store_host, STORE_HTTP_PORT),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/compute", post(compute))
        .route("/store/{key}", get(store_get).put(store_put))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", GATEWAY_HTTP_PORT);
    tracing::info!("Gateway service listening on {}", addr);
    let listener = TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
