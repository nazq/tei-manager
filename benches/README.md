# Multiplexer Benchmarks

Benchmarks measuring gRPC multiplexer overhead by comparing direct TEI instance calls vs routed calls through tei-manager.

## Benchmark Groups

| Group | Description |
|-------|-------------|
| `embedding_overhead` | Single embedding requests at various input lengths |
| `concurrent_requests` | Parallel requests (5, 10, 20 concurrent) |
| `streaming_requests` | Streaming RPC batches (5, 10, 20 items) |
| `arrow_batch` | Arrow IPC batch embedding (1, 10, 50, 100 rows) |

## Quick Start

```bash
# Start benchmark environment (tei-manager + TEI instance)
just bench-start

# Run benchmarks
just bench

# Stop environment
just bench-stop
```

## Available Commands

```bash
just bench-start      # Start tei-manager + bench-instance on ports 9000/9001/8081
just bench            # Run full benchmark suite
just bench-quick      # Quick run without saving
just bench-baseline   # Save baseline for comparison
just bench-compare    # Compare against saved baseline
just bench-open       # Run and open HTML report
just bench-stop       # Stop benchmark environment
just bench-status     # Check if environment is running
```

## Endpoints

When `bench-start` is running:

| Service | Endpoint |
|---------|----------|
| tei-manager API | http://localhost:9000 |
| gRPC Multiplexer | http://localhost:9001 |
| TEI Instance (direct) | http://localhost:8081 |

## Results (v0.6.0)

Measured on NVIDIA RTX GPU with `BAAI/bge-small-en-v1.5` model.

### Single Request Latency

| Input Size | Direct | Multiplexer | Overhead |
|------------|--------|-------------|----------|
| short (~2 tokens) | 1.12 ms | 1.26 ms | +12.5% |
| medium (~20 tokens) | 1.11 ms | 1.33 ms | +20% |
| long (~100 tokens) | 1.39 ms | 1.59 ms | +14% |
| extra-long (~500 tokens) | 1.97 ms | 2.22 ms | +13% |

### Concurrent Requests (medium text)

| Concurrency | Direct | Multiplexer | Overhead |
|-------------|--------|-------------|----------|
| 5 | 1.72 ms | 1.85 ms | +7.5% |
| 10 | 1.92 ms | 2.08 ms | +8% |
| 20 | 2.31 ms | 2.73 ms | +18% |

### Streaming Batches

| Batch Size | Direct | Multiplexer | Overhead |
|------------|--------|-------------|----------|
| 5 | 1.80 ms | 2.04 ms | +13% |
| 10 | 1.93 ms | 2.22 ms | +15% |
| 20 | 2.14 ms | 2.39 ms | +12% |

### Arrow IPC Batch Embedding

| Batch Size | Arrow IPC | Streaming | Arrow Advantage |
|------------|-----------|-----------|-----------------|
| 1 | 1.28 ms | 1.18 ms | −8% (overhead) |
| 10 | 2.26 ms | 2.21 ms | −2% |
| 50 | 3.08 ms | 2.97 ms | −4% |
| 100 | 3.76 ms | 3.80 ms | ~0% |

Arrow IPC shows similar performance to streaming at batch sizes up to 100. The benefit of Arrow comes from reduced serialization overhead at very large batch sizes (1000+) and columnar data interoperability.

## Interpreting Results

### Expected Overhead

The multiplexer adds one gRPC hop (client → multiplexer → TEI), which introduces:
- Connection routing (~50-100μs)
- Request/response forwarding
- Target extraction from metadata

**Typical overhead: 7-20%** depending on:
- Input size (smaller inputs = higher % overhead)
- Concurrency (batching amortizes overhead)
- GPU inference time (longer inference = lower % overhead)

### Variance

Benchmarks may show ±5% variance between runs due to:
- GPU thermal throttling
- System load
- Network jitter (even on localhost)

For reliable comparisons, use `just bench-baseline` and `just bench-compare`.

## Manual Setup

If you need custom configuration:

```bash
# 1. Start tei-manager
cargo run --release -- -c config/tei-manager.toml

# 2. Create benchmark instance
curl -X POST http://localhost:9000/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bench-instance",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8081
  }'

# 3. Wait for instance to be ready
curl http://localhost:9000/instances/bench-instance

# 4. Run benchmarks
cargo bench --bench multiplexer_overhead
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
- Historical comparisons
- Statistical analysis

## Troubleshooting

**"Connection refused" errors:**
```bash
just bench-status  # Check if environment is running
just bench-start   # Start it if not
```

**High variance in results:**
- Close other applications
- Ensure GPU isn't thermal throttling: `nvidia-smi -q -d TEMPERATURE`
- Increase sample size in benchmark code

**Benchmarks timeout:**
- Check TEI instance health: `curl http://localhost:8081/health`
- Check logs: `cat /tmp/tei-manager-bench.log`
