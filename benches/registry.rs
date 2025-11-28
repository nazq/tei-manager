//! Registry contention and port allocation benchmarks
//!
//! Benchmarks for registry operations including:
//! - Single-threaded add/remove
//! - Multi-threaded read-heavy (90% list, 10% add)
//! - Multi-threaded write-heavy (50% add, 50% remove)
//! - Port allocation latency

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::collections::HashSet;
use std::hint::black_box;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Instant;
use tei_manager::{InstanceConfig, Registry};
use tokio::runtime::Runtime;

/// Create a registry with pre-populated instances
async fn create_populated_registry(count: usize) -> Arc<Registry> {
    // Use a high port range to avoid conflicts
    let base_port = 30000u16;
    let registry = Arc::new(Registry::new(
        None,
        "text-embeddings-router".to_string(),
        base_port,
        base_port + count as u16 + 1000,
    ));

    for i in 0..count {
        let config = InstanceConfig {
            name: format!("instance-{}", i),
            model_id: "test-model".to_string(),
            port: base_port + i as u16,
            ..Default::default()
        };
        registry.add(config).await.unwrap();
    }

    registry
}

/// Benchmark registry list operation
fn bench_registry_list(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("registry_list");

    for instance_count in [10, 100, 1000] {
        let registry = rt.block_on(create_populated_registry(instance_count));

        group.bench_with_input(
            BenchmarkId::new("instances", instance_count),
            &registry,
            |b, registry| {
                b.to_async(&rt).iter(|| async {
                    let _list = registry.list().await;
                });
            },
        );
    }
    group.finish();
}

/// Benchmark registry get operation
fn bench_registry_get(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("registry_get");

    for instance_count in [10, 100, 1000] {
        let registry = rt.block_on(create_populated_registry(instance_count));

        group.bench_with_input(
            BenchmarkId::new("instances", instance_count),
            &registry,
            |b, registry| {
                b.to_async(&rt).iter(|| async {
                    let _instance = registry.get(black_box("instance-0")).await;
                });
            },
        );
    }
    group.finish();
}

/// Benchmark registry add/remove cycle
fn bench_registry_add_remove(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("registry_add_remove");
    group.sample_size(50);

    for instance_count in [10, 100] {
        group.bench_with_input(
            BenchmarkId::new("base_instances", instance_count),
            &instance_count,
            |b, &instance_count| {
                b.to_async(&rt).iter_custom(|iters| async move {
                    let registry = create_populated_registry(instance_count).await;
                    let start = Instant::now();

                    for i in 0..iters {
                        let name = format!("temp-{}", i);
                        let config = InstanceConfig {
                            name: name.clone(),
                            model_id: "test-model".to_string(),
                            port: 40000 + (i % 1000) as u16,
                            ..Default::default()
                        };
                        let _ = registry.add(config).await;
                        let _ = registry.remove(&name).await;
                    }

                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

/// Benchmark concurrent reads (90% list, 10% get)
fn bench_registry_concurrent_reads(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("registry_90_read_10_get");
    group.sample_size(50);

    for concurrency in [10, 50, 100] {
        let registry = rt.block_on(create_populated_registry(100));

        group.bench_with_input(
            BenchmarkId::new("readers", concurrency),
            &(registry, concurrency),
            |b, (registry, concurrency)| {
                b.to_async(&rt).iter(|| {
                    let reg = registry.clone();
                    async move {
                        let handles: Vec<_> = (0..*concurrency)
                            .map(|i| {
                                let reg = reg.clone();
                                tokio::spawn(async move {
                                    if i % 10 == 0 {
                                        let _ = reg.get(&format!("instance-{}", i % 100)).await;
                                    } else {
                                        let _ = reg.list().await;
                                    }
                                })
                            })
                            .collect();
                        futures::future::join_all(handles).await
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark write contention (readers don't block under write contention test)
fn bench_registry_write_contention(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("registry_write_contention");
    group.sample_size(30);

    for writers in [1, 2, 5] {
        let registry = rt.block_on(create_populated_registry(50));

        group.bench_with_input(
            BenchmarkId::new("writers", writers),
            &(registry, writers),
            |b, (registry, writers)| {
                b.to_async(&rt).iter_custom(|iters| {
                    let reg = registry.clone();
                    let writers = *writers;
                    async move {
                        let start = Instant::now();

                        // Spawn writer tasks
                        let writer_handles: Vec<_> = (0..writers)
                            .map(|w| {
                                let reg = reg.clone();
                                tokio::spawn(async move {
                                    for i in 0..(iters / writers as u64) {
                                        let name = format!("writer-{}-{}", w, i);
                                        let config = InstanceConfig {
                                            name: name.clone(),
                                            model_id: "test".to_string(),
                                            port: 45000 + (w as u16 * 1000) + (i % 500) as u16,
                                            ..Default::default()
                                        };
                                        let _ = reg.add(config).await;
                                        let _ = reg.remove(&name).await;
                                    }
                                })
                            })
                            .collect();

                        // Spawn concurrent reader
                        let reader_reg = reg.clone();
                        let reader_handle = tokio::spawn(async move {
                            for _ in 0..iters {
                                let _ = reader_reg.list().await;
                            }
                        });

                        // Wait for all
                        futures::future::join_all(writer_handles).await;
                        reader_handle.await.unwrap();

                        start.elapsed()
                    }
                });
            },
        );
    }
    group.finish();
}

// ============================================================================
// Port Allocation Benchmarks
// ============================================================================

/// Find a free port in the given range (standalone function for benchmarking)
fn find_free_port_in_range_bench(
    search_start: u16,
    range_start: u16,
    range_end: u16,
    used_ports: &HashSet<u16>,
) -> Option<u16> {
    // Search from search_start to range_end
    (search_start..range_end)
        .find(|port| !used_ports.contains(port) && TcpListener::bind(("0.0.0.0", *port)).is_ok())
        .or_else(|| {
            // Wrap around: search from range_start to search_start
            (range_start..search_start).find(|port| {
                !used_ports.contains(port) && TcpListener::bind(("0.0.0.0", *port)).is_ok()
            })
        })
}

/// Benchmark port allocation with empty used set
fn bench_port_allocation_empty(c: &mut Criterion) {
    let mut group = c.benchmark_group("port_allocation");

    for range_size in [10, 100, 1000] {
        let base_port = 50000u16;
        let used_ports: HashSet<u16> = HashSet::new();

        group.bench_with_input(
            BenchmarkId::new("range_size_empty", range_size),
            &(base_port, range_size, used_ports),
            |b, (base_port, range_size, used_ports)| {
                b.iter(|| {
                    find_free_port_in_range_bench(
                        black_box(*base_port),
                        *base_port,
                        base_port + *range_size,
                        used_ports,
                    )
                });
            },
        );
    }
    group.finish();
}

/// Benchmark port allocation with partially used range
fn bench_port_allocation_partial(c: &mut Criterion) {
    let mut group = c.benchmark_group("port_allocation_partial");

    for fill_percent in [25, 50, 75, 90] {
        let base_port = 51000u16;
        let range_size = 100u16;
        let fill_count = (range_size as usize * fill_percent) / 100;

        // Mark first N ports as used
        let used_ports: HashSet<u16> = (0..fill_count as u16).map(|i| base_port + i).collect();

        group.bench_with_input(
            BenchmarkId::new("fill_percent", fill_percent),
            &(base_port, range_size, used_ports),
            |b, (base_port, range_size, used_ports)| {
                b.iter(|| {
                    find_free_port_in_range_bench(
                        black_box(*base_port),
                        *base_port,
                        base_port + *range_size,
                        used_ports,
                    )
                });
            },
        );
    }
    group.finish();
}

/// Benchmark port allocation with wraparound
fn bench_port_allocation_wraparound(c: &mut Criterion) {
    let mut group = c.benchmark_group("port_allocation_wrap");

    let base_port = 52000u16;
    let range_size = 100u16;

    // All ports used except the first few
    let used_ports: HashSet<u16> = (10..range_size).map(|i| base_port + i).collect();

    // Start search near end of range to force wraparound
    let search_start = base_port + 95;

    group.bench_function("wraparound", |b| {
        b.iter(|| {
            find_free_port_in_range_bench(
                black_box(search_start),
                base_port,
                base_port + range_size,
                &used_ports,
            )
        });
    });

    group.finish();
}

/// Benchmark TCP bind overhead (the core operation in port allocation)
fn bench_tcp_bind(c: &mut Criterion) {
    let mut group = c.benchmark_group("tcp_bind");

    // Find a free port to test with
    let test_port = (53000u16..54000)
        .find(|&p| TcpListener::bind(("0.0.0.0", p)).is_ok())
        .expect("No free port found for testing");

    group.bench_function("single_bind_check", |b| {
        b.iter(|| {
            // Note: This creates and immediately drops the listener
            let _ = TcpListener::bind(("0.0.0.0", black_box(test_port)));
        });
    });

    // Benchmark sequential bind checks
    group.bench_function("10_sequential_binds", |b| {
        b.iter(|| {
            for port in test_port..(test_port + 10) {
                let _ = TcpListener::bind(("0.0.0.0", black_box(port)));
            }
        });
    });

    group.finish();
}

/// Benchmark showing scaling of registry with instance count
fn bench_registry_scaling(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("registry_scaling");

    for instance_count in [10, 50, 100, 500, 1000] {
        let registry = rt.block_on(create_populated_registry(instance_count));

        group.bench_with_input(
            BenchmarkId::new("list_with_instances", instance_count),
            &registry,
            |b, registry| {
                b.to_async(&rt).iter(|| async {
                    let list = registry.list().await;
                    black_box(list.len())
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_registry_list,
    bench_registry_get,
    bench_registry_add_remove,
    bench_registry_concurrent_reads,
    bench_registry_write_contention,
    bench_port_allocation_empty,
    bench_port_allocation_partial,
    bench_port_allocation_wraparound,
    bench_tcp_bind,
    bench_registry_scaling,
);
criterion_main!(benches);
