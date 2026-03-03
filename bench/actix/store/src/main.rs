use std::sync::Arc;

use actix_web::{web, App, HttpResponse, HttpServer};
use bench_actix_common::{StoreResponse, StoreValue};
use dashmap::DashMap;
use tracing::info;

struct AppState {
    store: DashMap<String, String>,
}

async fn health() -> HttpResponse {
    HttpResponse::Ok().body("ok")
}

async fn get_key(
    path: web::Path<String>,
    data: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let key = path.into_inner();
    match data.store.get(&key) {
        Some(value) => HttpResponse::Ok().json(StoreResponse {
            key,
            value: value.clone(),
        }),
        None => HttpResponse::NotFound().body("key not found"),
    }
}

async fn put_key(
    path: web::Path<String>,
    body: web::Json<StoreValue>,
    data: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let key = path.into_inner();
    let value = body.into_inner().value;
    data.store.insert(key.clone(), value.clone());
    HttpResponse::Ok().json(StoreResponse { key, value })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();
    info!("store service starting on :8082");

    let state = web::Data::new(Arc::new(AppState {
        store: DashMap::new(),
    }));

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/health", web::get().to(health))
            .route("/store/{key}", web::get().to(get_key))
            .route("/store/{key}", web::put().to(put_key))
    })
    .bind("0.0.0.0:8082")?
    .run()
    .await
}
