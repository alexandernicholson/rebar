use actix_web::{web, App, HttpResponse, HttpServer};
use bench_actix_common::{fib, ComputeRequest, ComputeResponse};
use tracing::info;

async fn health() -> HttpResponse {
    HttpResponse::Ok().body("ok")
}

async fn compute(body: web::Json<ComputeRequest>) -> HttpResponse {
    let n = body.n;
    if n < 0 || n > 92 {
        return HttpResponse::InternalServerError().body("n must be between 0 and 92");
    }
    let n = n as u64;
    let result = tokio::task::spawn_blocking(move || fib(n))
        .await
        .unwrap();
    HttpResponse::Ok().json(ComputeResponse { result })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();
    info!("compute service starting on :8081");

    HttpServer::new(|| {
        App::new()
            .route("/health", web::get().to(health))
            .route("/compute", web::post().to(compute))
    })
    .bind("0.0.0.0:8081")?
    .run()
    .await
}
