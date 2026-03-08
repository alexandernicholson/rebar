use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::runtime::Runtime as TokioRuntime;

use rebar_core::runtime::Runtime;

fn tokio_rt() -> TokioRuntime {
    TokioRuntime::new().unwrap()
}

/// Benchmark: single process spawn latency
fn bench_spawn_single(c: &mut Criterion) {
    let tokio_rt = tokio_rt();
    c.bench_function("process_lifecycle/spawn_single", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let rt = Runtime::new(1);
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                rt.spawn(move |_ctx| async move {
                    let _ = done_tx.send(());
                })
                .await;
                done_rx.await.unwrap();
            });
        });
    });
}

/// Benchmark: batch spawn N processes
fn bench_spawn_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_lifecycle/spawn_batch");

    for count in [10, 100, 1_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |b, &count| {
                let tokio_rt = tokio_rt();
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let rt = Runtime::new(1);
                        let (done_tx, mut done_rx) =
                            tokio::sync::mpsc::channel::<()>(count);

                        for _ in 0..count {
                            let tx = done_tx.clone();
                            rt.spawn(move |_ctx| async move {
                                let _ = tx.send(()).await;
                            })
                            .await;
                        }
                        drop(done_tx);

                        for _ in 0..count {
                            done_rx.recv().await.unwrap();
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: spawn + teardown (process exits and is cleaned from table)
fn bench_spawn_teardown(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_lifecycle/spawn_teardown");

    for count in [10, 100, 500] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |b, &count| {
                let tokio_rt = tokio_rt();
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let rt = Runtime::new(1);

                        for _ in 0..count {
                            rt.spawn(|_ctx| async {}).await;
                        }

                        // Wait for all processes to be cleaned up
                        // by checking the process table empties
                        let table = rt.table();
                        let start = std::time::Instant::now();
                        while !table.is_empty()
                            && start.elapsed() < std::time::Duration::from_secs(5)
                        {
                            tokio::task::yield_now().await;
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: spawn with immediate message send (realistic usage)
fn bench_spawn_and_send(c: &mut Criterion) {
    let tokio_rt = tokio_rt();
    c.bench_function("process_lifecycle/spawn_and_send", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let rt = Runtime::new(1);
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();

                let pid = rt
                    .spawn(move |mut ctx| async move {
                        ctx.recv().await.unwrap();
                        let _ = done_tx.send(());
                    })
                    .await;

                rt.send(pid, rmpv::Value::Nil).await.unwrap();
                done_rx.await.unwrap();
            });
        });
    });
}

criterion_group!(
    benches,
    bench_spawn_single,
    bench_spawn_batch,
    bench_spawn_teardown,
    bench_spawn_and_send
);
criterion_main!(benches);
