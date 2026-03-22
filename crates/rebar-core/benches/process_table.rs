use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use rebar_core::process::mailbox::Mailbox;
use rebar_core::process::table::{ProcessHandle, ProcessTable};
use rebar_core::process::{Message, ProcessId};

/// Benchmark: `allocate_pid` throughput (`AtomicU64` contention)
fn bench_allocate_pid(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_table/allocate_pid");

    group.bench_function("single_thread", |b| {
        let table = ProcessTable::new(1);
        b.iter(|| {
            table.allocate_pid();
        });
    });

    // Multi-threaded contention
    for n_threads in [2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::new("contended", n_threads),
            &n_threads,
            |b, &n_threads| {
                let table = Arc::new(ProcessTable::new(1));
                b.iter(|| {
                    let mut handles = Vec::new();
                    for _ in 0..n_threads {
                        let t = Arc::clone(&table);
                        handles.push(std::thread::spawn(move || {
                            for _ in 0..100 {
                                t.allocate_pid();
                            }
                        }));
                    }
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: insert throughput
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_table/insert");

    for count in [100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(u64::try_from(count).unwrap()));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |b, &count| {
                b.iter(|| {
                    let table = ProcessTable::new(1);
                    for _ in 0..count {
                        let pid = table.allocate_pid();
                        let (tx, _rx) = Mailbox::unbounded();
                        table.insert(pid, ProcessHandle::new(tx));
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: lookup (get) throughput
fn bench_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_table/lookup");

    for table_size in [100, 1_000, 10_000] {
        // Pre-populate table
        let table = Arc::new(ProcessTable::new(1));
        let mut pids = Vec::new();
        for _ in 0..table_size {
            let pid = table.allocate_pid();
            let (tx, _rx) = Mailbox::unbounded();
            table.insert(pid, ProcessHandle::new(tx));
            pids.push(pid);
        }

        group.bench_with_input(
            BenchmarkId::new("get", table_size),
            &pids,
            |b, pids| {
                let mut idx = 0;
                b.iter(|| {
                    let pid = pids[idx % pids.len()];
                    let _ = table.get(&pid);
                    idx += 1;
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: send-to-process lookup latency
fn bench_send(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_table/send");

    for table_size in [100, 1_000] {
        let table = Arc::new(ProcessTable::new(1));
        let mut pids = Vec::new();
        for _ in 0..table_size {
            let pid = table.allocate_pid();
            let (tx, _rx) = Mailbox::unbounded();
            table.insert(pid, ProcessHandle::new(tx));
            pids.push(pid);
        }

        group.bench_with_input(
            BenchmarkId::new("send", table_size),
            &pids,
            |b, pids| {
                let from = ProcessId::new(1, 0);
                let mut idx = 0;
                b.iter(|| {
                    let pid = pids[idx % pids.len()];
                    let msg = Message::new_internal(from, rmpv::Value::Nil);
                    let _ = table.send(pid, msg);
                    idx += 1;
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: concurrent read/write (insert + lookup mixed)
fn bench_concurrent_rw(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_table/concurrent_rw");

    for n_threads in [2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::from_parameter(n_threads),
            &n_threads,
            |b, &n_threads| {
                b.iter(|| {
                    let table = Arc::new(ProcessTable::new(1));
                    // Pre-populate with some entries
                    let mut pids = Vec::new();
                    for _ in 0..100 {
                        let pid = table.allocate_pid();
                        let (tx, _rx) = Mailbox::unbounded();
                        table.insert(pid, ProcessHandle::new(tx));
                        pids.push(pid);
                    }
                    let pids = Arc::new(pids);

                    let mut handles = Vec::new();
                    for t in 0..n_threads {
                        let table = Arc::clone(&table);
                        let pids = Arc::clone(&pids);
                        handles.push(std::thread::spawn(move || {
                            for i in 0..100 {
                                if i % 2 == 0 {
                                    // Write: insert new entry
                                    let pid = table.allocate_pid();
                                    let (tx, _rx) = Mailbox::unbounded();
                                    table.insert(pid, ProcessHandle::new(tx));
                                } else {
                                    // Read: lookup existing entry
                                    let pid = pids[(t * 100 + i) % pids.len()];
                                    let _ = table.get(&pid);
                                }
                            }
                        }));
                    }
                    for h in handles {
                        h.join().unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_allocate_pid,
    bench_insert,
    bench_lookup,
    bench_send,
    bench_concurrent_rw
);
criterion_main!(benches);
