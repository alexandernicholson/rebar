use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App, HttpResponse, HttpServer};
use bench_actix_common::{ComputeRequest, StoreValue};
use tracing::info;

struct AppState {
    client: reqwest::Client,
    compute_url: String,
    store_url: String,
}

async fn health(data: web::Data<Arc<AppState>>) -> HttpResponse {
    let compute_health = data
        .client
        .get(format!("{}/health", data.compute_url))
        .send()
        .await;
    let store_health = data
        .client
        .get(format!("{}/health", data.store_url))
        .send()
        .await;

    match (compute_health, store_health) {
        (Ok(c), Ok(s)) if c.status().is_success() && s.status().is_success() => {
            HttpResponse::Ok().body("ok")
        }
        _ => HttpResponse::ServiceUnavailable().body("unhealthy"),
    }
}

async fn compute(
    body: web::Json<ComputeRequest>,
    data: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let resp = data
        .client
        .post(format!("{}/compute", data.compute_url))
        .json(&body.into_inner())
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.bytes().await.unwrap_or_default();
            HttpResponse::build(actix_web::http::StatusCode::from_u16(status.as_u16()).unwrap())
                .content_type("application/json")
                .body(body)
        }
        Err(e) => {
            HttpResponse::BadGateway().body(format!("compute service error: {e}"))
        }
    }
}

async fn get_key(
    path: web::Path<String>,
    data: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let key = path.into_inner();
    let resp = data
        .client
        .get(format!("{}/store/{}", data.store_url, key))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.bytes().await.unwrap_or_default();
            HttpResponse::build(actix_web::http::StatusCode::from_u16(status.as_u16()).unwrap())
                .content_type("application/json")
                .body(body)
        }
        Err(e) => {
            HttpResponse::BadGateway().body(format!("store service error: {e}"))
        }
    }
}

async fn put_key(
    path: web::Path<String>,
    body: web::Json<StoreValue>,
    data: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let key = path.into_inner();
    let resp = data
        .client
        .put(format!("{}/store/{}", data.store_url, key))
        .json(&body.into_inner())
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.bytes().await.unwrap_or_default();
            HttpResponse::build(actix_web::http::StatusCode::from_u16(status.as_u16()).unwrap())
                .content_type("application/json")
                .body(body)
        }
        Err(e) => {
            HttpResponse::BadGateway().body(format!("store service error: {e}"))
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();

    let compute_host = std::env::var("COMPUTE_HOST").unwrap_or_else(|_| "compute".to_string());
    let store_host = std::env::var("STORE_HOST").unwrap_or_else(|_| "store".to_string());

    let compute_url = format!("http://{}:8081", compute_host);
    let store_url = format!("http://{}:8082", store_host);

    info!("gateway starting on :8080, compute={compute_url}, store={store_url}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    let state = web::Data::new(Arc::new(AppState {
        client,
        compute_url,
        store_url,
    }));

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/health", web::get().to(health))
            .route("/compute", web::post().to(compute))
            .route("/store/{key}", web::get().to(get_key))
            .route("/store/{key}", web::put().to(put_key))
    })
    .bind("0.0.0.0:8080")?
    .run()
    .await
}
