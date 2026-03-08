use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::runtime::Runtime as TokioRuntime;

use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::{
    start_supervisor, ChildEntry, ChildSpec, RestartStrategy, RestartType, SupervisorSpec,
};

fn tokio_rt() -> TokioRuntime {
    TokioRuntime::new().unwrap()
}

/// Benchmark: supervisor startup with N children
fn bench_supervisor_startup(c: &mut Criterion) {
    let mut group = c.benchmark_group("supervisor/startup");

    for n_children in [1, 5, 10, 50] {
        group.throughput(Throughput::Elements(n_children as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(n_children),
            &n_children,
            |b, &n_children| {
                let tokio_rt = tokio_rt();
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let rt = Arc::new(Runtime::new(1));
                        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
                        let children: Vec<ChildEntry> = (0..n_children)
                            .map(|i| {
                                ChildEntry::new(
                                    ChildSpec::new(format!("child-{}", i)),
                                    || async {
                                        tokio::time::sleep(Duration::from_secs(60)).await;
                                        ExitReason::Normal
                                    },
                                )
                            })
                            .collect();

                        let handle = start_supervisor(rt, spec, children).await;
                        // Brief wait for children to actually start
                        tokio::task::yield_now().await;
                        handle.shutdown();
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: dynamic add_child on running supervisor
fn bench_add_child(c: &mut Criterion) {
    let mut group = c.benchmark_group("supervisor/add_child");

    for count in [1, 10, 50] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |b, &count| {
                let tokio_rt = tokio_rt();
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let rt = Arc::new(Runtime::new(1));
                        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
                        let handle = start_supervisor(rt, spec, vec![]).await;

                        for i in 0..count {
                            let entry = ChildEntry::new(
                                ChildSpec::new(format!("dynamic-{}", i)),
                                || async {
                                    tokio::time::sleep(Duration::from_secs(60)).await;
                                    ExitReason::Normal
                                },
                            );
                            handle.add_child(entry).await.unwrap();
                        }

                        handle.shutdown();
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: restart latency (OneForOne — child crashes, measured until restart)
fn bench_restart_one_for_one(c: &mut Criterion) {
    let tokio_rt = tokio_rt();
    c.bench_function("supervisor/restart_one_for_one", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let rt = Arc::new(Runtime::new(1));
                let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
                    .max_restarts(100)
                    .max_seconds(60);

                use std::sync::atomic::{AtomicU32, Ordering};
                let start_count = Arc::new(AtomicU32::new(0));
                let sc = Arc::clone(&start_count);
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let ready_tx = Arc::new(tokio::sync::Mutex::new(Some(ready_tx)));

                let children = vec![ChildEntry::new(
                    ChildSpec::new("crasher").restart(RestartType::Permanent),
                    move || {
                        let sc = Arc::clone(&sc);
                        let ready_tx = Arc::clone(&ready_tx);
                        async move {
                            let n = sc.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                // First start: crash immediately
                                ExitReason::Abnormal("crash".into())
                            } else {
                                // Restarted: signal ready
                                if let Some(tx) = ready_tx.lock().await.take() {
                                    let _ = tx.send(());
                                }
                                tokio::time::sleep(Duration::from_secs(60)).await;
                                ExitReason::Normal
                            }
                        }
                    },
                )];

                let handle = start_supervisor(rt, spec, children).await;
                // Wait for the restart to complete
                tokio::time::timeout(Duration::from_secs(2), ready_rx)
                    .await
                    .unwrap()
                    .unwrap();

                handle.shutdown();
                tokio::time::sleep(Duration::from_millis(10)).await;
            });
        });
    });
}

criterion_group!(
    benches,
    bench_supervisor_startup,
    bench_add_child,
    bench_restart_one_for_one
);
criterion_main!(benches);
