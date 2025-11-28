//! Connection pool benchmarks
//!
//! Benchmarks for connection pool operations including:
//! - Pool get latency (miss - no cached connection)
//! - DashMap concurrent access patterns
//! - Connection pruning overhead

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;
use tokio::runtime::Runtime;

/// Simulated connection entry for benchmarking DashMap operations
#[derive(Clone)]
#[allow(dead_code)]
struct MockConnection {
    name: String,
    created_at: Instant,
    last_used: Instant,
}

impl MockConnection {
    fn new(name: &str) -> Self {
        let now = Instant::now();
        Self {
            name: name.to_string(),
            created_at: now,
            last_used: now,
        }
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }
}

/// Create a mock pool with pre-populated connections
fn create_mock_pool(count: usize) -> Arc<DashMap<String, MockConnection>> {
    let pool = Arc::new(DashMap::new());
    for i in 0..count {
        let name = format!("instance-{}", i);
        pool.insert(name.clone(), MockConnection::new(&name));
    }
    pool
}

/// Benchmark pool get (hit - connection exists)
fn bench_pool_get_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_get_hit");

    for pool_size in [10, 100, 1000] {
        let pool = create_mock_pool(pool_size);

        group.bench_with_input(
            BenchmarkId::new("pool_size", pool_size),
            &pool,
            |b, pool| {
                b.iter(|| {
                    // Access existing connection (hit)
                    let conn = pool.get(black_box("instance-0"));
                    black_box(conn);
                });
            },
        );
    }
    group.finish();
}

/// Benchmark pool get with touch (simulates real usage pattern)
fn bench_pool_get_and_touch(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_get_touch");

    for pool_size in [10, 100, 1000] {
        let pool = create_mock_pool(pool_size);

        group.bench_with_input(
            BenchmarkId::new("pool_size", pool_size),
            &pool,
            |b, pool| {
                b.iter(|| {
                    // Get mutable reference and update timestamp
                    if let Some(mut entry) = pool.get_mut(black_box("instance-0")) {
                        entry.touch();
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark pool insert (miss - new connection)
fn bench_pool_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_insert");

    for pool_size in [10, 100, 1000] {
        group.bench_with_input(
            BenchmarkId::new("pool_size", pool_size),
            &pool_size,
            |b, &pool_size| {
                b.iter_custom(|iters| {
                    let pool = create_mock_pool(pool_size);
                    let start = Instant::now();
                    for i in 0..iters {
                        let name = format!("new-instance-{}", i);
                        pool.insert(name.clone(), MockConnection::new(&name));
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

/// Benchmark pool remove
fn bench_pool_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_remove");

    for pool_size in [10, 100, 1000] {
        group.bench_with_input(
            BenchmarkId::new("pool_size", pool_size),
            &pool_size,
            |b, &pool_size| {
                b.iter_custom(|iters| {
                    // Create pool with enough entries to remove
                    let pool = create_mock_pool(pool_size + iters as usize);
                    let start = Instant::now();
                    for i in 0..iters {
                        pool.remove(&format!("instance-{}", pool_size + i as usize));
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

/// Benchmark concurrent reads (simulates multiple handlers accessing pool)
fn bench_pool_concurrent_reads(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("pool_concurrent_reads");
    group.sample_size(50);

    for concurrency in [10, 50, 100] {
        let pool = create_mock_pool(100);

        group.bench_with_input(
            BenchmarkId::new("readers", concurrency),
            &(pool, concurrency),
            |b, (pool, concurrency)| {
                b.to_async(&rt).iter(|| async {
                    let handles: Vec<_> = (0..*concurrency)
                        .map(|i| {
                            let pool = pool.clone();
                            let key = format!("instance-{}", i % 100);
                            tokio::spawn(async move {
                                let _conn = pool.get(&key);
                            })
                        })
                        .collect();
                    futures::future::join_all(handles).await
                });
            },
        );
    }
    group.finish();
}

/// Benchmark mixed read/write (90% read, 10% write)
fn bench_pool_mixed_workload(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("pool_mixed_90_10");
    group.sample_size(50);

    for concurrency in [10, 50, 100] {
        let pool = create_mock_pool(100);

        group.bench_with_input(
            BenchmarkId::new("operations", concurrency),
            &(pool, concurrency),
            |b, (pool, concurrency)| {
                b.to_async(&rt).iter(|| async {
                    let handles: Vec<_> = (0..*concurrency)
                        .map(|i| {
                            let pool = pool.clone();
                            tokio::spawn(async move {
                                if i % 10 == 0 {
                                    // Write operation (10%)
                                    let name = format!("temp-{}", i);
                                    pool.insert(name.clone(), MockConnection::new(&name));
                                    pool.remove(&name);
                                } else {
                                    // Read operation (90%)
                                    let key = format!("instance-{}", i % 100);
                                    let _conn = pool.get(&key);
                                }
                            })
                        })
                        .collect();
                    futures::future::join_all(handles).await
                });
            },
        );
    }
    group.finish();
}

/// Benchmark pruning iteration (collecting idle connections)
fn bench_pool_prune_iteration(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_prune_iteration");

    for pool_size in [10, 100, 1000] {
        let pool = create_mock_pool(pool_size);

        group.bench_with_input(
            BenchmarkId::new("pool_size", pool_size),
            &pool,
            |b, pool| {
                b.iter(|| {
                    // Simulate pruning: iterate and collect keys
                    let _keys: Vec<String> = pool.iter().map(|e| e.key().clone()).collect();
                });
            },
        );
    }
    group.finish();
}

/// Benchmark entry API (used for atomic get-or-insert)
fn bench_pool_entry_api(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_entry_api");

    for pool_size in [10, 100, 1000] {
        let pool = create_mock_pool(pool_size);

        // Test entry API with existing key (occupied)
        group.bench_with_input(BenchmarkId::new("occupied", pool_size), &pool, |b, pool| {
            b.iter(|| {
                let entry = pool.entry(black_box("instance-0".to_string()));
                match entry {
                    dashmap::mapref::entry::Entry::Occupied(e) => {
                        black_box(e.get());
                    }
                    dashmap::mapref::entry::Entry::Vacant(_) => {}
                }
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_pool_get_hit,
    bench_pool_get_and_touch,
    bench_pool_insert,
    bench_pool_remove,
    bench_pool_concurrent_reads,
    bench_pool_mixed_workload,
    bench_pool_prune_iteration,
    bench_pool_entry_api,
);
criterion_main!(benches);
