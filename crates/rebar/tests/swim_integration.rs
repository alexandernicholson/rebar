//! End-to-end SWIM failure detection over the real TCP transport.
//!
//! Two `DistributedRuntime`s probe each other across TCP via the wire protocol.
//! We assert they discover each other as Alive, then that one node dying is
//! detected and the survivor marks it Dead — exercising the full path:
//! `swim_tick` -> `ConnectionManager` -> `TcpTransport` -> peer accept loop ->
//! `handle_inbound_frame` -> SWIM service -> Ack -> back over TCP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rebar::DistributedRuntime;
use rebar_cluster::connection::manager::{ConnectionManager, TransportConnector};
use rebar_cluster::swim::{NodeState, SwimConfig};
use rebar_cluster::transport::tcp::TcpTransport;
use rebar_cluster::transport::{TransportConnection, TransportError, TransportListener};
use tokio::sync::Mutex;

struct TcpConnector;

#[async_trait::async_trait]
impl TransportConnector for TcpConnector {
    async fn connect(
        &self,
        addr: SocketAddr,
    ) -> Result<Box<dyn TransportConnection>, TransportError> {
        let conn = TcpTransport::new().connect(addr).await?;
        Ok(Box::new(conn))
    }
}

fn fast_config() -> SwimConfig {
    SwimConfig::builder()
        .protocol_period(Duration::from_millis(20))
        .suspect_timeout(Duration::from_millis(200))
        .indirect_probe_count(0) // a single missed probe suspects
        .max_gossip_per_tick(16)
        .build()
}

/// Drive a node: accept inbound connections (feeding frames to the runtime)
/// and tick SWIM periodically, until `alive` is cleared.
fn run_node(
    rt: Arc<Mutex<DistributedRuntime>>,
    listener: rebar_cluster::transport::tcp::TcpTransportListener,
    alive: Arc<AtomicBool>,
) {
    // Accept loop.
    let accept_rt = Arc::clone(&rt);
    let accept_alive = Arc::clone(&alive);
    tokio::spawn(async move {
        let listener = Arc::new(listener);
        while accept_alive.load(Ordering::SeqCst) {
            let Ok(mut conn) = listener.accept().await else {
                break;
            };
            let conn_rt = Arc::clone(&accept_rt);
            let conn_alive = Arc::clone(&accept_alive);
            tokio::spawn(async move {
                while conn_alive.load(Ordering::SeqCst) {
                    match conn.recv().await {
                        Ok(frame) => {
                            // A "dead" node stops responding to anything.
                            if !conn_alive.load(Ordering::SeqCst) {
                                break;
                            }
                            let _ = conn_rt.lock().await.handle_inbound_frame(&frame).await;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // Tick loop.
    tokio::spawn(async move {
        while alive.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if !alive.load(Ordering::SeqCst) {
                break;
            }
            let _ = rt.lock().await.swim_tick().await;
        }
    });
}

async fn state_of(rt: &Arc<Mutex<DistributedRuntime>>, node: u64) -> Option<NodeState> {
    let guard = rt.lock().await;
    guard.swim().and_then(|s| s.state_of(node))
}

/// Poll `cond` until it holds or the timeout elapses.
async fn wait_until<F, Fut>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_detect_liveness_and_death_over_tcp() {
    let transport = TcpTransport::new();
    let listener_a = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let listener_b = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr_a = listener_a.local_addr();
    let addr_b = listener_b.local_addr();

    // Node 1 (A).
    let mut drt_a = DistributedRuntime::new(1, ConnectionManager::new(Box::new(TcpConnector)));
    drt_a.enable_swim(addr_a, fast_config());
    drt_a.swim_add_seed(2, addr_b);
    let rt_a = Arc::new(Mutex::new(drt_a));
    let alive_a = Arc::new(AtomicBool::new(true));

    // Node 2 (B).
    let mut drt_b = DistributedRuntime::new(2, ConnectionManager::new(Box::new(TcpConnector)));
    drt_b.enable_swim(addr_b, fast_config());
    drt_b.swim_add_seed(1, addr_a);
    let rt_b = Arc::new(Mutex::new(drt_b));
    let alive_b = Arc::new(AtomicBool::new(true));

    run_node(Arc::clone(&rt_a), listener_a, Arc::clone(&alive_a));
    run_node(Arc::clone(&rt_b), listener_b, Arc::clone(&alive_b));

    // 1. Both discover each other as Alive.
    let discovered = wait_until(Duration::from_secs(5), || async {
        state_of(&rt_a, 2).await == Some(NodeState::Alive)
            && state_of(&rt_b, 1).await == Some(NodeState::Alive)
    })
    .await;
    assert!(discovered, "nodes should discover each other as Alive over TCP");

    // 2. Kill B. It stops probing and stops responding.
    alive_b.store(false, Ordering::SeqCst);

    // 3. A detects B's death: Suspect -> Dead.
    let detected_dead = wait_until(Duration::from_secs(5), || async {
        state_of(&rt_a, 2).await == Some(NodeState::Dead)
    })
    .await;
    assert!(
        detected_dead,
        "surviving node should mark the dead peer Dead, got {:?}",
        state_of(&rt_a, 2).await
    );

    alive_a.store(false, Ordering::SeqCst);
}
