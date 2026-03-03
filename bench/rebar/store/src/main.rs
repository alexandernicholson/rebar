use std::collections::HashMap;
use std::sync::Arc;

use axum::{Json, Router, extract::{Path, State}, routing::get};
use bench_common::{STORE_HTTP_PORT, StoreResponse, StoreValue};
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::{RestartStrategy, SupervisorSpec, start_supervisor};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};

enum KeyCommand {
    Get {
        reply: oneshot::Sender<Option<String>>,
    },
    Put {
        value: String,
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
    keys: Arc<Mutex<HashMap<String, mpsc::Sender<KeyCommand>>>>,
}

impl AppState {
    /// Get or create a process for the given key.
    async fn get_key_sender(&self, key: &str) -> mpsc::Sender<KeyCommand> {
        let mut keys = self.keys.lock().await;
        if let Some(tx) = keys.get(key) {
            return tx.clone();
        }

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<KeyCommand>(64);

        self.runtime.spawn(move |_ctx| async move {
            let mut value: Option<String> = None;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    KeyCommand::Get { reply } => {
                        let _ = reply.send(value.clone());
                    }
                    KeyCommand::Put { value: v, reply } => {
                        value = Some(v);
                        let _ = reply.send(());
                    }
                }
            }
        }).await;

        keys.insert(key.to_string(), cmd_tx.clone());
        cmd_tx
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn store_get(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<StoreResponse>, axum::http::StatusCode> {
    let sender = state.get_key_sender(&key).await;
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = KeyCommand::Get { reply: reply_tx };

    if sender.send(cmd).await.is_err() {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    match reply_rx.await {
        Ok(Some(value)) => Ok(Json(StoreResponse { key, value })),
        Ok(None) => Err(axum::http::StatusCode::NOT_FOUND),
        Err(_) => Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn store_put(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<StoreValue>,
) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
    let sender = state.get_key_sender(&key).await;
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = KeyCommand::Put {
        value: body.value,
        reply: reply_tx,
    };

    if sender.send(cmd).await.is_err() {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    match reply_rx.await {
        Ok(()) => Ok(axum::http::StatusCode::OK),
        Err(_) => Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let runtime = Arc::new(Runtime::new(2));

    // Start a supervisor for the store service
    let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
        .max_restarts(100)
        .max_seconds(60);
    let _supervisor = start_supervisor(runtime.clone(), spec, vec![]).await;

    let state = AppState {
        runtime,
        keys: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/store/{key}", get(store_get).put(store_put))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", STORE_HTTP_PORT);
    tracing::info!("Store service listening on {}", addr);
    let listener = TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
