use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::runtime::Runtime as TokioRuntime;

use rebar_core::runtime::Runtime;

fn tokio_rt() -> TokioRuntime {
    TokioRuntime::new().unwrap()
}

/// Benchmark: single pair ping-pong (send + recv round trip)
fn bench_ping_pong(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_passing/ping_pong");

    for count in [100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |b, &count| {
                let tokio_rt = tokio_rt();
                let rt = tokio_rt.block_on(async { Runtime::new(1) });
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

                        let receiver = rt
                            .spawn(move |mut ctx| async move {
                                for _ in 0..count {
                                    ctx.recv().await.unwrap();
                                }
                                let _ = done_tx.send(());
                            })
                            .await;

                        for _ in 0..count {
                            rt.send(receiver, rmpv::Value::Nil).await.unwrap();
                        }

                        done_rx.await.unwrap();
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: fan-out (1 sender -> N receivers)
fn bench_fan_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_passing/fan_out");

    for n_receivers in [10, 50, 100] {
        group.throughput(Throughput::Elements(n_receivers as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(n_receivers),
            &n_receivers,
            |b, &n_receivers| {
                let tokio_rt = tokio_rt();
                let rt = tokio_rt.block_on(async { Runtime::new(1) });
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let (done_tx, mut done_rx) =
                            tokio::sync::mpsc::channel::<()>(n_receivers);

                        let mut receivers = Vec::new();
                        for _ in 0..n_receivers {
                            let tx = done_tx.clone();
                            let pid = rt
                                .spawn(move |mut ctx| async move {
                                    ctx.recv().await.unwrap();
                                    let _ = tx.send(()).await;
                                })
                                .await;
                            receivers.push(pid);
                        }
                        drop(done_tx);

                        for pid in &receivers {
                            rt.send(*pid, rmpv::Value::Nil).await.unwrap();
                        }

                        for _ in 0..n_receivers {
                            done_rx.recv().await.unwrap();
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: fan-in (N senders -> 1 receiver)
fn bench_fan_in(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_passing/fan_in");

    for n_senders in [10, 50, 100] {
        group.throughput(Throughput::Elements(n_senders as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(n_senders),
            &n_senders,
            |b, &n_senders| {
                let tokio_rt = tokio_rt();
                let rt = tokio_rt.block_on(async { Runtime::new(1) });
                b.iter(|| {
                    tokio_rt.block_on(async {
                        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

                        let receiver = rt
                            .spawn(move |mut ctx| async move {
                                for _ in 0..n_senders {
                                    ctx.recv().await.unwrap();
                                }
                                let _ = done_tx.send(());
                            })
                            .await;

                        for _ in 0..n_senders {
                            let dest = receiver;
                            rt.spawn(move |ctx| async move {
                                ctx.send(dest, rmpv::Value::Nil).await.unwrap();
                            })
                            .await;
                        }

                        done_rx.await.unwrap();
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: message size impact
fn bench_message_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_passing/message_size");

    let payloads: Vec<(&str, rmpv::Value)> = vec![
        ("nil", rmpv::Value::Nil),
        ("small_string", rmpv::Value::String("hello".into())),
        (
            "1kb_binary",
            rmpv::Value::Binary(vec![0u8; 1024]),
        ),
        (
            "64kb_binary",
            rmpv::Value::Binary(vec![0u8; 65536]),
        ),
    ];

    for (name, payload) in payloads {
        group.bench_with_input(BenchmarkId::new("send_recv", name), &payload, |b, payload| {
            let tokio_rt = tokio_rt();
            let rt = tokio_rt.block_on(async { Runtime::new(1) });
            let payload = payload.clone();
            b.iter(|| {
                tokio_rt.block_on(async {
                    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                    let payload_clone = payload.clone();

                    let receiver = rt
                        .spawn(move |mut ctx| async move {
                            ctx.recv().await.unwrap();
                            let _ = done_tx.send(());
                        })
                        .await;

                    rt.send(receiver, payload_clone).await.unwrap();
                    done_rx.await.unwrap();
                });
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_ping_pong,
    bench_fan_out,
    bench_fan_in,
    bench_message_sizes
);
criterion_main!(benches);
