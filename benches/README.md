# Benchmarks

Performance benchmarks for tei-manager components and TEI integration.

## Benchmark Suites

| Suite | Description | External Deps |
|-------|-------------|---------------|
| `embedding` | Arrow IPC serialization/deserialization | None |
| `pool` | Connection pool (DashMap) operations | None |
| `registry` | Instance registry operations | None |
| `multiplexer_overhead` | End-to-end TEI latency | Testcontainers (Docker) |

## Quick Start

```bash
# Run all benchmarks (multiplexer_overhead will start TEI container automatically)
just bench

# Run only local benchmarks (no Docker needed)
just bench-local

# Open HTML report after running
just bench-open
```

## Results Summary (v0.8.0)

Measured on Intel CPU with `BAAI/bge-small-en-v1.5` model (CPU inference via testcontainers).

### Pool Operations (DashMap)

| Operation | Pool Size | Latency |
|-----------|-----------|---------|
| get (hit) | 10-1000 | **12 ns** |
| get + touch | 10-1000 | **25 ns** |
| insert | 10-1000 | **135 ns** |
| entry API | 10-1000 | **17 ns** |
| concurrent reads (100) | 100 | 77 µs |

**Verdict:** O(1) performance regardless of pool size. Not a bottleneck.

### Registry Operations

| Operation | Instances | Latency |
|-----------|-----------|---------|
| get | 10-1000 | **39 ns** |
| list | 10 | 111 ns |
| list | 100 | 821 ns |
| list | 1000 | 8.3 µs |
| add/remove | 10-100 | 2.1 µs |
| write contention (5 writers) | | ~1 ms |

**Verdict:** Get is O(1). List scales linearly. Write contention includes TCP bind check (~1.6µs/port).

### Arrow IPC

| Operation | Batch Size | Latency | Throughput |
|-----------|------------|---------|------------|
| serialize | 100 | 1.5 µs | 65 M items/s |
| serialize | 1000 | 6.8 µs | 147 M items/s |
| serialize | 10000 | 48 µs | 208 M items/s |
| deserialize | 100 | 0.98 µs | 102 M items/s |
| deserialize | 1000 | 3.4 µs | 294 M items/s |
| deserialize | 10000 | 30.6 µs | 327 M items/s |
| roundtrip | 100 | 15.8 µs | |
| roundtrip | 1000 | 161 µs | |
| roundtrip | 10000 | 1.68 ms | |

**Verdict:** Arrow IPC is highly efficient. 10K items round-trip in <2ms. Scales linearly.

### TEI Latency (CPU, via testcontainers)

| Benchmark | Input | Latency |
|-----------|-------|---------|
| embed/short | "Hello world" | **3.5 ms** |
| embed/medium | ~100 chars | **4.6 ms** |
| embed/long | ~600 chars | **15 ms** |
| concurrent/2 | | 7.8 ms |
| concurrent/5 | | 15.3 ms |
| concurrent/10 | | 21.5 ms |
| streaming/5 | | 13.6 ms |
| streaming/10 | | 18.7 ms |
| streaming/20 | | 31 ms |

**Note:** These are CPU inference times. GPU inference is 3-10x faster.

### TCP Port Binding

| Operation | Latency |
|-----------|---------|
| single bind check | 1.6 µs |
| 10 sequential binds | 15.9 µs |

**Verdict:** Port availability check is ~1.6µs. Not a bottleneck.

## Key Takeaways

1. **Pool is not a bottleneck** - 12ns get, perfect O(1) scaling
2. **Registry is fast** - 39ns get, list is O(n) but acceptable
3. **Arrow IPC overhead is minimal** - sub-ms for typical batch sizes
4. **TEI inference dominates latency** - 3.5-15ms per request (CPU)
5. **Write contention ~1ms** - TCP bind check in hot path, acceptable for instance creation

## Available Commands

```bash
just bench           # Run all benchmarks
just bench-local     # Run local benchmarks only (no Docker)
just bench-quick     # Quick run with fewer samples
just bench-baseline  # Save baseline for comparison
just bench-compare   # Compare against saved baseline
just bench-open      # Run and open HTML report
just bench-clean     # Remove benchmark results
```

## HTML Reports

Criterion generates detailed HTML reports:

```bash
# After running benchmarks
open target/criterion/report/index.html
```

Reports include:
- Latency distributions
- Throughput graphs
- Historical comparisons (if baseline saved)
- Statistical analysis

## Troubleshooting

**Docker errors during multiplexer_overhead:**
```bash
# Ensure Docker is running
docker ps

# Check for leftover containers
docker ps -a | grep text-embeddings
```

**High variance in results:**
- Close other applications
- Increase sample size: edit `group.sample_size(N)` in bench code
- Run multiple times and compare

**Benchmarks take too long:**
```bash
# Use quick mode
just bench-quick
```
