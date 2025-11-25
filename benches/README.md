# Multiplexer Overhead Benchmark

This benchmark measures the overhead introduced by the gRPC multiplexer layer by comparing:
- **Direct**: Direct gRPC calls to a TEI instance
- **Multiplexer**: gRPC calls routed through the tei-manager multiplexer

## Prerequisites

- NVIDIA GPU with CUDA support
- `text-embeddings-router` binary in PATH
- `nvidia-smi` available
- Model downloaded (default: `BAAI/bge-small-en-v1.5`)

## Running the Benchmark

### Automatic (Recommended)

The script handles all setup and teardown:

```bash
./bench-multiplexer.sh
```

This will:
1. Start a direct TEI instance on port 8080
2. Start tei-manager with multiplexer on port 9090
3. Create and start a benchmark instance
4. Run criterion benchmarks comparing direct vs multiplexer
5. Clean up all processes

### Custom Model

```bash
MODEL_ID="sentence-transformers/all-MiniLM-L6-v2" ./bench-multiplexer.sh
```

### Manual Setup

If you want to run the benchmark manually:

1. **Start direct TEI instance:**
   ```bash
   text-embeddings-router --model-id BAAI/bge-small-en-v1.5 --port 8080
   ```

2. **Start tei-manager:**
   ```bash
   cargo run --release -- --port 3000 --grpc-port 9090
   ```

3. **Create benchmark instance:**
   ```bash
   curl -X POST http://localhost:3000/instances \
     -H "Content-Type: application/json" \
     -d '{
       "name": "bench-instance",
       "model_id": "BAAI/bge-small-en-v1.5",
       "port": 8081,
       "gpu_id": 0
     }'

   curl -X POST http://localhost:3000/instances/bench-instance/start
   ```

4. **Run benchmark:**
   ```bash
   cargo bench --bench multiplexer_overhead
   ```

## Understanding the Results

The benchmark tests three input sizes:
- **short**: "Hello world" (~2 tokens)
- **medium**: ~20 tokens
- **long**: ~100 tokens (repeated Lorem ipsum)

### Output

Criterion will show:
- Mean latency for each case
- Standard deviation
- Comparison between direct and multiplexer

Example output:
```
embedding_overhead/direct/short   time: [1.23 ms 1.25 ms 1.27 ms]
embedding_overhead/multiplexer/short   time: [1.45 ms 1.47 ms 1.49 ms]
                                   change: [+17.2% +17.6% +18.0%]
```

### Expected Overhead

The multiplexer adds:
- 1 additional gRPC hop (client → multiplexer → TEI)
- Request routing logic
- Stream forwarding

Typical overhead: **5-20%** depending on:
- Input size (smaller inputs see higher % overhead)
- Network conditions
- GPU inference time (longer inference = lower % overhead)

## Analyzing Results

Results are saved to `target/criterion/embedding_overhead/`:
- HTML reports with graphs
- Raw data in JSON format
- Comparison data for trend analysis

View the HTML report:
```bash
open target/criterion/embedding_overhead/report/index.html
```

## Troubleshooting

**GPU not found:**
- Ensure `nvidia-smi` works
- Check CUDA drivers are installed

**Ports in use:**
- Change ports in script or kill existing processes
- Check with: `lsof -i :8080,9090,3000`

**Benchmark fails to connect:**
- Ensure all services are running and healthy
- Check logs with `journalctl` or process output
- Verify with: `curl http://localhost:8080/health`

**High variance:**
- Run on idle machine (close other applications)
- Increase sample size in `bench_embedding_overhead`
- Disable GPU boost: `sudo nvidia-smi -pm 1`
