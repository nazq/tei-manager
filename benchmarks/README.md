# HuggingFace Embedding Benchmarks

Standalone benchmark suite for measuring single-query embedding latency on CPU vs GPU. Helps determine when local computation is faster than remote TEI API calls.

## Quick Start

```bash
# Quick benchmark (5 models, 100 iterations)
uv run benchmarks/hf_embedding_bench.py

# Full benchmark (20 models, 10 iterations)
uv run benchmarks/hf_embedding_bench.py --full

# Dry run - verify model metadata without timing
uv run benchmarks/hf_embedding_bench.py --full --dry-run

# Export results
uv run benchmarks/hf_embedding_bench.py --fmt json --fmt csv --fmt md
```

## What It Measures

For each model, the benchmark measures:

- **Model load time** - Time to load model from disk/cache to memory (CPU or GPU)
- **Embedding latency** - Time to embed a single query (not batch)
- **CPU vs GPU comparison** - Side-by-side timing with speedup factor

Three query lengths are tested:
- **Short**: "machine learning basics" (3 words)
- **Medium**: ~15 words typical search query
- **Long**: ~50 words detailed question

## CLI Options

| Option | Description | Default |
|--------|-------------|---------|
| `-n, --iterations` | Timed iterations per config | 100 (quick), 10 (full) |
| `-w, --warmup` | Warmup iterations | 10 (quick), 3 (full) |
| `--full` | Run with 20 diverse models | Off (5 models) |
| `--dry-run` | Load models, show info, skip timing | Off |
| `-f, --fmt` | Output format (json, csv, md) | None (terminal only) |

Multiple formats can be specified: `--fmt json --fmt csv --fmt md`

## Models

### Quick Mode (5 models)

| Label | Model | Type |
|-------|-------|------|
| small | all-MiniLM-L6-v2 | Dense |
| medium | all-mpnet-base-v2 | Dense |
| large | bge-large-en-v1.5 | Dense |
| sparse-small | Splade_PP_en_v1 | Sparse |
| sparse-large | efficient-splade-VI-BT-large-query | Sparse |

### Full Mode (20 models)

Includes diverse model sizes and architectures:

- **Tiny** (~22M params): MiniLM-L6, MiniLM-L3
- **Small** (~33M params): MiniLM-L12, BGE-small
- **Medium** (~110M params): MPNet, BGE-base, GTE-base, E5-base
- **Large** (~335M params): BGE-large, GTE-large, E5-large
- **Multilingual**: MiniLM-multi, E5-multi
- **Specialized**: Jina-code, Jina-long, Nomic-matryoshka
- **Sparse**: SPLADE variants

## Output

Results are written to `benchmarks/results/`:

- `hf_embedding_bench.json` - Full data for programmatic analysis
- `hf_embedding_bench.csv` - Spreadsheet-friendly format
- `hf_embedding_bench.md` - Human-readable report (committed to repo)

See [results/hf_embedding_bench.md](results/hf_embedding_bench.md) for latest benchmark results.

## Customizing

### Adding Models

Edit the model dictionaries in `hf_embedding_bench.py`:

```python
# Quick mode models (line ~52)
QUICK_MODELS: dict[str, dict[str, str]] = {
    "dense": {
        "small": "sentence-transformers/all-MiniLM-L6-v2",
        # Add more here...
    },
    "sparse": {
        "sparse-small": "prithivida/Splade_PP_en_v1",
    },
}

# Full mode models (line ~66)
FULL_MODELS: dict[str, dict[str, str]] = {
    "dense": {
        "my-new-model": "org/model-name",
        # ...
    },
    "sparse": { ... },
}
```

The size label (dict key) is used for grouping in reports. Use descriptive prefixes like `tiny-`, `small-`, `medium-`, `large-`, `xl-` for proper sorting.

### Adding Query Lengths

Edit `TEST_QUERIES` (~line 100):

```python
TEST_QUERIES: dict[str, str] = {
    "short": "machine learning basics",
    "medium": "What are the best practices for...",
    "long": "I'm looking for comprehensive documentation...",
    # Add more:
    "code": "def fibonacci(n: int) -> int:",
}
```

### Changing Defaults

Default iterations and warmup are set in `main()` based on mode:

```python
# In main() around line 1440
if iterations is None:
    iterations = 10 if full else 100  # Change these
if warmup is None:
    warmup = 3 if full else 10  # Change these
```

## Architecture

```
hf_embedding_bench.py
├── Configuration (QUICK_MODELS, FULL_MODELS, TEST_QUERIES)
├── Data Classes
│   ├── ModelInfo - HuggingFace model metadata
│   ├── TimingResult - Raw timing data per config
│   ├── ComparisonRow - CPU vs GPU side-by-side
│   ├── QuerySizeAggregation - Averages by query length
│   └── ModelSizeAggregation - Averages by model size
├── Benchmark Engine
│   ├── load_dense_model() - SentenceTransformer loading
│   ├── load_sparse_model() - SPLADE/MLM loading
│   ├── benchmark_model() - Full timing run
│   └── dry_run_model() - Metadata only
├── Output Formatters
│   ├── build_*_table() - Rich terminal tables
│   └── export_*(json|csv|markdown)
└── CLI (Typer)
```

## Dependencies

Managed via inline script metadata (PEP 723):

```python
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "torch>=2.0.0",
#     "transformers>=4.36.0",
#     "sentence-transformers>=2.2.0",
#     "rich>=13.0.0",
#     "typer>=0.9.0",
#     "einops>=0.7.0",
# ]
# ///
```

No separate requirements file needed - `uv run` handles everything.

## Notes

- First run downloads models to HuggingFace cache (~/.cache/huggingface/)
- GPU memory usage varies by model (7B models need ~16GB VRAM)
- Load times are from cache, not initial download
- Sparse models use AutoModelForMaskedLM, not SentenceTransformer
