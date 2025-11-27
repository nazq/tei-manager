#!/usr/bin/env python3
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
"""
HuggingFace Embedding Benchmark: CPU vs GPU latency for search queries.

Measures single-query embedding latency to help decide when remote TEI calls
are worth it vs local computation.

Usage:
    uv run benchmarks/hf_embedding_bench.py
    uv run benchmarks/hf_embedding_bench.py --iterations 500 --warmup 20
    uv run benchmarks/hf_embedding_bench.py --fmt json --fmt csv --fmt md
"""

from __future__ import annotations

import json
import platform
import statistics
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.progress import (
    BarColumn,
    Progress,
    SpinnerColumn,
    TaskProgressColumn,
    TextColumn,
    TimeElapsedColumn,
)
from rich.table import Table

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Quick mode models (default) - 5 models for fast benchmarking
QUICK_MODELS: dict[str, dict[str, str]] = {
    "dense": {
        "small": "sentence-transformers/all-MiniLM-L6-v2",
        "medium": "sentence-transformers/all-mpnet-base-v2",
        "large": "BAAI/bge-large-en-v1.5",
    },
    "sparse": {
        "sparse-small": "prithivida/Splade_PP_en_v1",
        "sparse-large": "naver/efficient-splade-VI-BT-large-query",
    },
}

# Full mode models - 20 diverse models for comprehensive benchmarking
FULL_MODELS: dict[str, dict[str, str]] = {
    "dense": {
        # Tiny (< 25M params)
        "tiny-minilm-l6": "sentence-transformers/all-MiniLM-L6-v2",
        "tiny-minilm-l3": "sentence-transformers/paraphrase-MiniLM-L3-v2",
        # Small (25-50M params)
        "small-minilm-l12": "sentence-transformers/all-MiniLM-L12-v2",
        "small-bge": "BAAI/bge-small-en-v1.5",
        # Medium (100-150M params)
        "medium-mpnet": "sentence-transformers/all-mpnet-base-v2",
        "medium-bge": "BAAI/bge-base-en-v1.5",
        "medium-gte": "thenlper/gte-base",
        "medium-e5": "intfloat/e5-base-v2",
        # Large (300-400M params)
        "large-bge": "BAAI/bge-large-en-v1.5",
        "large-gte": "thenlper/gte-large",
        "large-e5": "intfloat/e5-large-v2",
        # Multilingual
        "multi-minilm": "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2",
        "multi-e5": "intfloat/multilingual-e5-base",
        # Specialized
        "code-jina": "jinaai/jina-embeddings-v2-base-code",
        "long-jina": "jinaai/jina-embeddings-v2-base-en",
        "matryoshka-nomic": "nomic-ai/nomic-embed-text-v1.5",
    },
    "sparse": {
        "sparse-small": "prithivida/Splade_PP_en_v1",
        "sparse-large": "naver/efficient-splade-VI-BT-large-query",
    },
}

# Test queries of varying lengths
TEST_QUERIES: dict[str, str] = {
    "short": "machine learning basics",
    "medium": "What are the best practices for implementing distributed systems with high availability?",
    "long": (
        "I'm looking for comprehensive documentation on how to design and implement "
        "a fault-tolerant microservices architecture that can handle millions of requests "
        "per second while maintaining sub-millisecond latency and ensuring data consistency "
        "across multiple geographic regions with automatic failover capabilities."
    ),
}


class OutputFormat(str, Enum):
    """Supported output formats."""

    JSON = "json"
    CSV = "csv"
    MD = "md"


@dataclass
class ModelInfo:
    """Model metadata pulled from HuggingFace config."""

    model_id: str
    model_size_label: str
    embedding_dim: int | None = None
    max_seq_length: int | None = None
    num_parameters: int | None = None
    model_type: str | None = None
    is_sparse: bool = False

    @property
    def params_str(self) -> str:
        """Format parameter count as human-readable string."""
        if self.num_parameters is None:
            return "Unknown"
        if self.num_parameters >= 1_000_000_000:
            return f"{self.num_parameters / 1_000_000_000:.1f}B"
        return f"{self.num_parameters / 1_000_000:.0f}M"


@dataclass
class TimingResult:
    """Timing results for a single configuration."""

    model_name: str
    model_size: str
    query_length: str
    device: str
    times_ms: list[float] = field(default_factory=list)
    model_info: ModelInfo | None = None
    load_time_ms: float | None = None

    @property
    def mean_ms(self) -> float:
        return statistics.mean(self.times_ms)

    @property
    def std_ms(self) -> float:
        return statistics.stdev(self.times_ms) if len(self.times_ms) > 1 else 0.0

    @property
    def p50_ms(self) -> float:
        return statistics.median(self.times_ms)

    @property
    def p95_ms(self) -> float:
        return (
            statistics.quantiles(self.times_ms, n=20)[18]
            if len(self.times_ms) >= 20
            else max(self.times_ms)
        )

    @property
    def p99_ms(self) -> float:
        return (
            statistics.quantiles(self.times_ms, n=100)[98]
            if len(self.times_ms) >= 100
            else max(self.times_ms)
        )


@dataclass
class HardwareInfo:
    """Hardware specifications for the benchmark environment."""

    cpu_model: str
    cpu_cores: int
    cpu_threads: int
    ram_gb: float
    gpu_model: str | None
    gpu_memory_gb: float | None
    cuda_version: str | None
    torch_version: str

    @classmethod
    def detect(cls) -> HardwareInfo:
        """Detect hardware specifications."""
        import torch

        # CPU info
        cpu_model = platform.processor() or "Unknown"
        try:
            # Try to get more detailed CPU info on Linux
            with open("/proc/cpuinfo") as f:
                for line in f:
                    if line.startswith("model name"):
                        cpu_model = line.split(":")[1].strip()
                        break
        except (FileNotFoundError, PermissionError):
            pass

        cpu_cores = 1
        cpu_threads = 1
        try:
            import os

            cpu_threads = os.cpu_count() or 1
            # Try to get physical cores on Linux
            try:
                with open(
                    "/sys/devices/system/cpu/cpu0/topology/core_siblings_list"
                ) as f:
                    siblings = f.read().strip()
                    # Parse range like "0-15" or list like "0,1,2,3"
                    if "-" in siblings:
                        parts = siblings.split("-")
                        threads_per_core = int(parts[1]) - int(parts[0]) + 1
                    else:
                        threads_per_core = len(siblings.split(","))
                    cpu_cores = (
                        cpu_threads // (threads_per_core // cpu_cores)
                        if threads_per_core > 0
                        else cpu_threads
                    )
            except (FileNotFoundError, PermissionError, ValueError):
                cpu_cores = cpu_threads  # Fallback
        except Exception:
            pass

        # RAM info
        ram_gb = 0.0
        try:
            with open("/proc/meminfo") as f:
                for line in f:
                    if line.startswith("MemTotal"):
                        ram_kb = int(line.split()[1])
                        ram_gb = ram_kb / 1024 / 1024
                        break
        except (FileNotFoundError, PermissionError):
            pass

        # GPU info
        gpu_model = None
        gpu_memory_gb = None
        cuda_version = None

        if torch.cuda.is_available():
            gpu_model = torch.cuda.get_device_name(0)
            gpu_memory_gb = torch.cuda.get_device_properties(0).total_memory / 1024**3
            cuda_version = torch.version.cuda

        return cls(
            cpu_model=cpu_model,
            cpu_cores=cpu_cores,
            cpu_threads=cpu_threads,
            ram_gb=ram_gb,
            gpu_model=gpu_model,
            gpu_memory_gb=gpu_memory_gb,
            cuda_version=cuda_version,
            torch_version=torch.__version__,
        )


@dataclass
class BenchmarkResults:
    """Collection of all benchmark results."""

    results: list[TimingResult] = field(default_factory=list)
    iterations: int = 100
    warmup: int = 10
    gpu_available: bool = False
    hardware: HardwareInfo | None = None
    run_timestamp: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    total_runtime_seconds: float = 0.0

    def add(self, result: TimingResult) -> None:
        self.results.append(result)


# ---------------------------------------------------------------------------
# Benchmark Engine
# ---------------------------------------------------------------------------


def get_model_info(
    model_id: str, model_size_label: str, is_sparse: bool = False
) -> ModelInfo:
    """Extract model metadata from HuggingFace config."""
    from transformers import AutoConfig

    info = ModelInfo(
        model_id=model_id,
        model_size_label=model_size_label,
        is_sparse=is_sparse,
    )

    try:
        config = AutoConfig.from_pretrained(model_id, trust_remote_code=True)

        # Embedding dimension
        info.embedding_dim = getattr(config, "hidden_size", None)

        # Max sequence length (various config keys)
        info.max_seq_length = (
            getattr(config, "max_position_embeddings", None)
            or getattr(config, "max_seq_length", None)
            or getattr(config, "n_positions", None)
        )

        # Model type/architecture
        info.model_type = getattr(config, "model_type", None)

    except Exception:
        pass  # Config not available, leave as None

    return info


def load_dense_model(
    model_id: str, model_size_label: str, device: str
) -> tuple[object, ModelInfo, float]:
    """Load a dense embedding model and extract metadata.

    Returns:
        Tuple of (model, model_info, load_time_ms)
    """
    from sentence_transformers import SentenceTransformer

    start = time.perf_counter()
    model = SentenceTransformer(model_id, device=device, trust_remote_code=True)
    load_time_ms = (time.perf_counter() - start) * 1000

    # Get model info
    info = get_model_info(model_id, model_size_label, is_sparse=False)

    # Get param count from loaded model
    try:
        info.num_parameters = sum(p.numel() for p in model.parameters())
    except Exception:
        pass

    # SentenceTransformer may have better embedding dim info
    try:
        info.embedding_dim = model.get_sentence_embedding_dimension()
    except Exception:
        pass

    # Max seq length from tokenizer
    try:
        info.max_seq_length = model.max_seq_length
    except Exception:
        pass

    return model, info, load_time_ms


def load_sparse_model(
    model_id: str, model_size_label: str, device: str
) -> tuple[tuple, ModelInfo, float]:
    """Load a sparse (SPLADE) embedding model and extract metadata.

    Returns:
        Tuple of (model_tuple, model_info, load_time_ms)
    """
    from transformers import AutoModelForMaskedLM, AutoTokenizer

    start = time.perf_counter()
    tokenizer = AutoTokenizer.from_pretrained(model_id)
    model = AutoModelForMaskedLM.from_pretrained(model_id)
    model = model.to(device)
    model.eval()
    load_time_ms = (time.perf_counter() - start) * 1000

    # Get model info
    info = get_model_info(model_id, model_size_label, is_sparse=True)

    # Get param count from loaded model
    try:
        info.num_parameters = sum(p.numel() for p in model.parameters())
    except Exception:
        pass

    # For sparse models, vocab size is effectively the "embedding dim"
    try:
        info.embedding_dim = model.config.vocab_size
    except Exception:
        pass

    return (model, tokenizer), info, load_time_ms


def embed_dense(model: object, query: str) -> None:
    """Generate dense embedding for a query."""
    model.encode(query, convert_to_tensor=True)  # type: ignore[union-attr]


def embed_sparse(model_tuple: tuple, query: str, device: str) -> None:
    """Generate sparse embedding for a query."""
    import torch

    model, tokenizer = model_tuple
    inputs = tokenizer(query, return_tensors="pt", padding=True, truncation=True)
    inputs = {k: v.to(device) for k, v in inputs.items()}

    with torch.no_grad():
        outputs = model(**inputs)
        # SPLADE: max pooling over sequence length, then ReLU + log
        _ = torch.max(
            torch.log1p(torch.relu(outputs.logits))
            * inputs["attention_mask"].unsqueeze(-1),
            dim=1,
        ).values


def dry_run_model(
    model_id: str,
    model_size: str,
    is_sparse: bool,
    progress: Progress,
    task_id: int,
) -> list[TimingResult]:
    """Load model on CPU, extract metadata, return placeholder results."""
    import torch

    results: list[TimingResult] = []

    # Load model on CPU only to get metadata
    if is_sparse:
        model, model_info, load_time_ms = load_sparse_model(model_id, model_size, "cpu")
    else:
        model, model_info, load_time_ms = load_dense_model(model_id, model_size, "cpu")

    # Create one result per query length (for report structure)
    for query_name in TEST_QUERIES:
        result = TimingResult(
            model_name=model_id,
            model_size=model_size,
            query_length=query_name,
            device="cpu",
            model_info=model_info,
            load_time_ms=load_time_ms,
        )
        result.times_ms = [0.0]
        results.append(result)

    progress.advance(task_id)

    # Cleanup
    del model
    torch.cuda.empty_cache()

    return results


def benchmark_model(
    model_id: str,
    model_size: str,
    device: str,
    iterations: int,
    warmup: int,
    is_sparse: bool,
    progress: Progress,
    task_id: int,
) -> list[TimingResult]:
    """Benchmark a single model across all query lengths."""
    import torch

    results: list[TimingResult] = []

    # Load model and get info
    if is_sparse:
        model, model_info, load_time_ms = load_sparse_model(model_id, model_size, device)
    else:
        model, model_info, load_time_ms = load_dense_model(model_id, model_size, device)

    for query_name, query_text in TEST_QUERIES.items():
        result = TimingResult(
            model_name=model_id,
            model_size=model_size,
            query_length=query_name,
            device=device,
            model_info=model_info,
            load_time_ms=load_time_ms,
        )

        # Warmup
        for _ in range(warmup):
            if is_sparse:
                embed_sparse(model, query_text, device)  # type: ignore[arg-type]
            else:
                embed_dense(model, query_text)

        # Sync before timing (GPU)
        if device == "cuda":
            torch.cuda.synchronize()

        # Timed iterations
        for _ in range(iterations):
            start = time.perf_counter()

            if is_sparse:
                embed_sparse(model, query_text, device)  # type: ignore[arg-type]
            else:
                embed_dense(model, query_text)

            if device == "cuda":
                torch.cuda.synchronize()

            elapsed_ms = (time.perf_counter() - start) * 1000
            result.times_ms.append(elapsed_ms)
            progress.advance(task_id)

        results.append(result)

    # Cleanup
    del model
    if device == "cuda":
        torch.cuda.empty_cache()

    return results


def run_benchmarks(
    iterations: int,
    warmup: int,
    console: Console,
    full_mode: bool = False,
    dry_run: bool = False,
) -> BenchmarkResults:
    """Run all benchmarks.

    Args:
        iterations: Number of timed iterations per benchmark.
        warmup: Number of warmup iterations.
        console: Rich console for output.
        full_mode: If True, use FULL_MODELS (20 models), else QUICK_MODELS (5).
        dry_run: If True, load models and show info but skip timing.
    """
    import torch

    gpu_available = torch.cuda.is_available()

    # Select model set based on mode
    model_set = FULL_MODELS if full_mode else QUICK_MODELS
    dense_models = model_set["dense"]
    sparse_models = model_set["sparse"]

    # Detect hardware
    hardware = HardwareInfo.detect()

    all_models = {**dense_models, **sparse_models}

    benchmark_results = BenchmarkResults(
        iterations=iterations,
        warmup=warmup,
        gpu_available=gpu_available,
        hardware=hardware,
    )

    mode_str = "FULL" if full_mode else "QUICK"
    dry_str = " (DRY RUN)" if dry_run else ""

    console.print(f"\n[bold blue]Mode: {mode_str}{dry_str}[/bold blue]")

    console.print("\n[bold blue]Hardware Configuration[/bold blue]")
    console.print(f"  CPU: {hardware.cpu_model}")
    console.print(f"  CPU Cores/Threads: {hardware.cpu_cores}/{hardware.cpu_threads}")
    console.print(f"  RAM: {hardware.ram_gb:.1f} GB")
    if hardware.gpu_model:
        console.print(f"  GPU: {hardware.gpu_model}")
        console.print(f"  GPU Memory: {hardware.gpu_memory_gb:.1f} GB")
        console.print(f"  CUDA: {hardware.cuda_version}")
    console.print(f"  PyTorch: {hardware.torch_version}")

    # Dry run: only load models on CPU to get metadata
    if dry_run:
        console.print("\n[bold blue]Dry Run - Loading models for metadata[/bold blue]")
        console.print(f"  Models: {len(all_models)}")
        console.print()

        with Progress(
            SpinnerColumn(),
            TextColumn("[progress.description]{task.description}"),
            BarColumn(),
            TaskProgressColumn(),
            TimeElapsedColumn(),
            console=console,
        ) as progress:
            main_task = progress.add_task(
                "[cyan]Loading models...", total=len(all_models)
            )

            # Dense models
            for size, model_id in dense_models.items():
                progress.update(main_task, description=f"[cyan]Loading {size}...")
                try:
                    results = dry_run_model(
                        model_id=model_id,
                        model_size=size,
                        is_sparse=False,
                        progress=progress,
                        task_id=main_task,
                    )
                    for r in results:
                        benchmark_results.add(r)
                except Exception as e:
                    console.print(f"[yellow]Skipping {model_id}: {e}[/yellow]")
                    progress.advance(main_task)

            # Sparse models
            for size, model_id in sparse_models.items():
                progress.update(main_task, description=f"[cyan]Loading {size}...")
                try:
                    results = dry_run_model(
                        model_id=model_id,
                        model_size=size,
                        is_sparse=True,
                        progress=progress,
                        task_id=main_task,
                    )
                    for r in results:
                        benchmark_results.add(r)
                except Exception as e:
                    console.print(f"[yellow]Skipping {model_id}: {e}[/yellow]")
                    progress.advance(main_task)

        return benchmark_results

    # Full benchmark run
    devices = ["cpu", "cuda"] if gpu_available else ["cpu"]
    total_configs = len(all_models) * len(devices) * len(TEST_QUERIES)
    total_iterations = total_configs * iterations

    console.print("\n[bold blue]Benchmark Configuration[/bold blue]")
    console.print(f"  Iterations: {iterations}")
    console.print(f"  Warmup: {warmup}")
    console.print(f"  GPU Available: {'Yes' if gpu_available else 'No'}")
    console.print(f"  Devices: {', '.join(devices)}")
    console.print(f"  Models: {len(all_models)}")
    console.print(f"  Query lengths: {', '.join(TEST_QUERIES.keys())}")
    console.print()

    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        BarColumn(),
        TaskProgressColumn(),
        TimeElapsedColumn(),
        console=console,
    ) as progress:
        main_task = progress.add_task(
            "[cyan]Running benchmarks...", total=total_iterations
        )

        for device in devices:
            # Dense models
            for size, model_id in dense_models.items():
                progress.update(
                    main_task, description=f"[cyan]{size} dense on {device}..."
                )
                try:
                    results = benchmark_model(
                        model_id=model_id,
                        model_size=size,
                        device=device,
                        iterations=iterations,
                        warmup=warmup,
                        is_sparse=False,
                        progress=progress,
                        task_id=main_task,
                    )
                    for r in results:
                        benchmark_results.add(r)
                except Exception as e:
                    console.print(
                        f"[yellow]Skipping {model_id} on {device}: {e}[/yellow]"
                    )
                    # Advance progress for skipped iterations
                    progress.advance(main_task, len(TEST_QUERIES) * iterations)

            # Sparse models
            for size, model_id in sparse_models.items():
                progress.update(main_task, description=f"[cyan]{size} on {device}...")
                try:
                    results = benchmark_model(
                        model_id=model_id,
                        model_size=size,
                        device=device,
                        iterations=iterations,
                        warmup=warmup,
                        is_sparse=True,
                        progress=progress,
                        task_id=main_task,
                    )
                    for r in results:
                        benchmark_results.add(r)
                except Exception as e:
                    console.print(
                        f"[yellow]Skipping {model_id} on {device}: {e}[/yellow]"
                    )
                    progress.advance(main_task, len(TEST_QUERIES) * iterations)

    return benchmark_results


# ---------------------------------------------------------------------------
# Data Aggregation
# ---------------------------------------------------------------------------


@dataclass
class ComparisonRow:
    """A single row comparing CPU vs GPU for a model+query combination."""

    model_name: str
    model_size: str
    query_length: str
    cpu_mean_ms: float
    cpu_p50_ms: float
    cpu_p95_ms: float
    cpu_p99_ms: float
    gpu_mean_ms: float | None
    gpu_p50_ms: float | None
    gpu_p95_ms: float | None
    gpu_p99_ms: float | None
    # Model metadata
    embedding_dim: int | None = None
    num_parameters: int | None = None
    max_seq_length: int | None = None
    model_type: str | None = None
    # Load times
    cpu_load_time_ms: float | None = None
    gpu_load_time_ms: float | None = None

    @property
    def speedup(self) -> float | None:
        if self.gpu_mean_ms and self.gpu_mean_ms > 0:
            return self.cpu_mean_ms / self.gpu_mean_ms
        return None

    @property
    def params_str(self) -> str:
        """Format parameter count as human-readable string."""
        if self.num_parameters is None:
            return "?"
        if self.num_parameters >= 1_000_000_000:
            return f"{self.num_parameters / 1_000_000_000:.1f}B"
        return f"{self.num_parameters / 1_000_000:.0f}M"

    @property
    def load_time_str(self) -> str:
        """Format load time as human-readable string (use CPU load time)."""
        load_ms = self.cpu_load_time_ms
        if load_ms is None:
            return "?"
        if load_ms >= 1000:
            return f"{load_ms / 1000:.1f}s"
        return f"{load_ms:.0f}ms"


@dataclass
class QuerySizeAggregation:
    """Aggregated stats for a query size across all models."""

    query_length: str
    cpu_mean_ms: float
    cpu_p50_ms: float
    cpu_p95_ms: float
    gpu_mean_ms: float | None
    gpu_p50_ms: float | None
    gpu_p95_ms: float | None

    @property
    def speedup(self) -> float | None:
        if self.gpu_mean_ms and self.gpu_mean_ms > 0:
            return self.cpu_mean_ms / self.gpu_mean_ms
        return None


@dataclass
class ModelSizeAggregation:
    """Aggregated stats for a model size across all query lengths."""

    model_size: str
    cpu_mean_ms: float
    cpu_p50_ms: float
    cpu_p95_ms: float
    gpu_mean_ms: float | None
    gpu_p50_ms: float | None
    gpu_p95_ms: float | None

    @property
    def speedup(self) -> float | None:
        if self.gpu_mean_ms and self.gpu_mean_ms > 0:
            return self.cpu_mean_ms / self.gpu_mean_ms
        return None


def build_comparison_data(results: BenchmarkResults) -> list[ComparisonRow]:
    """Build CPU vs GPU comparison rows for each model+query."""
    cpu_results: dict[tuple[str, str, str], TimingResult] = {}
    gpu_results: dict[tuple[str, str, str], TimingResult] = {}

    for r in results.results:
        key = (r.model_name, r.model_size, r.query_length)
        if r.device == "cpu":
            cpu_results[key] = r
        else:
            gpu_results[key] = r

    rows: list[ComparisonRow] = []
    for key, cpu_r in cpu_results.items():
        gpu_r = gpu_results.get(key)
        # Get model info from CPU result (same model)
        info = cpu_r.model_info
        rows.append(
            ComparisonRow(
                model_name=key[0],
                model_size=key[1],
                query_length=key[2],
                cpu_mean_ms=cpu_r.mean_ms,
                cpu_p50_ms=cpu_r.p50_ms,
                cpu_p95_ms=cpu_r.p95_ms,
                cpu_p99_ms=cpu_r.p99_ms,
                gpu_mean_ms=gpu_r.mean_ms if gpu_r else None,
                gpu_p50_ms=gpu_r.p50_ms if gpu_r else None,
                gpu_p95_ms=gpu_r.p95_ms if gpu_r else None,
                gpu_p99_ms=gpu_r.p99_ms if gpu_r else None,
                embedding_dim=info.embedding_dim if info else None,
                num_parameters=info.num_parameters if info else None,
                max_seq_length=info.max_seq_length if info else None,
                model_type=info.model_type if info else None,
                cpu_load_time_ms=cpu_r.load_time_ms,
                gpu_load_time_ms=gpu_r.load_time_ms if gpu_r else None,
            )
        )

    # Sort by model size order, then query length
    # Support both quick mode labels and full mode labels
    size_order = {
        # Quick mode
        "small": 10,
        "medium": 20,
        "large": 30,
        # Full mode - tiny
        "tiny-minilm-l6": 1,
        "tiny-minilm-l3": 2,
        # Full mode - small
        "small-minilm-l12": 11,
        "small-bge": 12,
        # Full mode - medium
        "medium-mpnet": 21,
        "medium-bge": 22,
        "medium-gte": 23,
        "medium-e5": 24,
        # Full mode - large
        "large-bge": 31,
        "large-gte": 32,
        "large-e5": 33,
        # Full mode - multilingual
        "multi-minilm": 40,
        "multi-e5": 41,
        # Full mode - specialized
        "code-jina": 50,
        "long-jina": 51,
        "matryoshka-nomic": 52,
        # Sparse (always last)
        "sparse-small": 100,
        "sparse-large": 101,
    }
    query_order = {"short": 0, "medium": 1, "long": 2}
    rows.sort(
        key=lambda x: (
            size_order.get(x.model_size, 99),
            query_order.get(x.query_length, 99),
        )
    )

    return rows


def build_query_aggregations(
    comparison_rows: list[ComparisonRow],
) -> list[QuerySizeAggregation]:
    """Aggregate results by query size across all models."""
    query_groups: dict[str, list[ComparisonRow]] = {}
    for row in comparison_rows:
        if row.query_length not in query_groups:
            query_groups[row.query_length] = []
        query_groups[row.query_length].append(row)

    aggregations: list[QuerySizeAggregation] = []
    for query_length in ["short", "medium", "long"]:
        if query_length not in query_groups:
            continue
        rows = query_groups[query_length]

        cpu_means = [r.cpu_mean_ms for r in rows]
        cpu_p50s = [r.cpu_p50_ms for r in rows]
        cpu_p95s = [r.cpu_p95_ms for r in rows]

        gpu_means = [r.gpu_mean_ms for r in rows if r.gpu_mean_ms is not None]
        gpu_p50s = [r.gpu_p50_ms for r in rows if r.gpu_p50_ms is not None]
        gpu_p95s = [r.gpu_p95_ms for r in rows if r.gpu_p95_ms is not None]

        aggregations.append(
            QuerySizeAggregation(
                query_length=query_length,
                cpu_mean_ms=statistics.mean(cpu_means),
                cpu_p50_ms=statistics.mean(cpu_p50s),
                cpu_p95_ms=statistics.mean(cpu_p95s),
                gpu_mean_ms=statistics.mean(gpu_means) if gpu_means else None,
                gpu_p50_ms=statistics.mean(gpu_p50s) if gpu_p50s else None,
                gpu_p95_ms=statistics.mean(gpu_p95s) if gpu_p95s else None,
            )
        )

    return aggregations


def build_model_aggregations(
    comparison_rows: list[ComparisonRow],
) -> list[ModelSizeAggregation]:
    """Aggregate results by model size across all query lengths."""
    model_groups: dict[str, list[ComparisonRow]] = {}
    for row in comparison_rows:
        if row.model_size not in model_groups:
            model_groups[row.model_size] = []
        model_groups[row.model_size].append(row)

    aggregations: list[ModelSizeAggregation] = []
    # Support both quick and full mode labels
    size_order = [
        # Quick mode
        "small",
        "medium",
        "large",
        # Full mode - tiny
        "tiny-minilm-l6",
        "tiny-minilm-l3",
        # Full mode - small
        "small-minilm-l12",
        "small-bge",
        # Full mode - medium
        "medium-mpnet",
        "medium-bge",
        "medium-gte",
        "medium-e5",
        # Full mode - large
        "large-bge",
        "large-gte",
        "large-e5",
        # Full mode - multilingual
        "multi-minilm",
        "multi-e5",
        # Full mode - specialized
        "code-jina",
        "long-jina",
        "matryoshka-nomic",
        # Sparse
        "sparse-small",
        "sparse-large",
    ]
    # Only iterate over sizes that exist in the data
    for model_size in size_order:
        if model_size not in model_groups:
            continue
        rows = model_groups[model_size]

        cpu_means = [r.cpu_mean_ms for r in rows]
        cpu_p50s = [r.cpu_p50_ms for r in rows]
        cpu_p95s = [r.cpu_p95_ms for r in rows]

        gpu_means = [r.gpu_mean_ms for r in rows if r.gpu_mean_ms is not None]
        gpu_p50s = [r.gpu_p50_ms for r in rows if r.gpu_p50_ms is not None]
        gpu_p95s = [r.gpu_p95_ms for r in rows if r.gpu_p95_ms is not None]

        aggregations.append(
            ModelSizeAggregation(
                model_size=model_size,
                cpu_mean_ms=statistics.mean(cpu_means),
                cpu_p50_ms=statistics.mean(cpu_p50s),
                cpu_p95_ms=statistics.mean(cpu_p95s),
                gpu_mean_ms=statistics.mean(gpu_means) if gpu_means else None,
                gpu_p50_ms=statistics.mean(gpu_p50s) if gpu_p50s else None,
                gpu_p95_ms=statistics.mean(gpu_p95s) if gpu_p95s else None,
            )
        )

    return aggregations


# ---------------------------------------------------------------------------
# Output Formatters
# ---------------------------------------------------------------------------


def build_comparison_table(
    comparison_rows: list[ComparisonRow], gpu_available: bool
) -> Table:
    """Build a rich table with CPU vs GPU side-by-side."""
    table = Table(
        title="CPU vs GPU Comparison - all times in ms (per model + query)",
        show_header=True,
        header_style="bold magenta",
    )

    table.add_column("Model", style="cyan", no_wrap=True)
    table.add_column("Size", style="green")
    table.add_column("Params", justify="right", style="dim")
    table.add_column("Dim", justify="right", style="dim")
    table.add_column("Query", style="yellow")
    table.add_column("CPU Mean", justify="right")
    table.add_column("CPU P50", justify="right")
    table.add_column("CPU P95", justify="right")
    if gpu_available:
        table.add_column("GPU Mean", justify="right")
        table.add_column("GPU P50", justify="right")
        table.add_column("GPU P95", justify="right")
        table.add_column("Speedup", justify="right", style="bold green")

    for row in comparison_rows:
        model_short = row.model_name.split("/")[-1][:20]
        dim_str = str(row.embedding_dim) if row.embedding_dim else "?"
        if gpu_available:
            speedup_str = f"{row.speedup:.1f}x" if row.speedup else "N/A"
            table.add_row(
                model_short,
                row.model_size,
                row.params_str,
                dim_str,
                row.query_length,
                f"{row.cpu_mean_ms:.2f}",
                f"{row.cpu_p50_ms:.2f}",
                f"{row.cpu_p95_ms:.2f}",
                f"{row.gpu_mean_ms:.2f}" if row.gpu_mean_ms else "N/A",
                f"{row.gpu_p50_ms:.2f}" if row.gpu_p50_ms else "N/A",
                f"{row.gpu_p95_ms:.2f}" if row.gpu_p95_ms else "N/A",
                speedup_str,
            )
        else:
            table.add_row(
                model_short,
                row.model_size,
                row.params_str,
                dim_str,
                row.query_length,
                f"{row.cpu_mean_ms:.2f}",
                f"{row.cpu_p50_ms:.2f}",
                f"{row.cpu_p95_ms:.2f}",
            )

    return table


def build_aggregation_table(
    aggregations: list[QuerySizeAggregation], gpu_available: bool
) -> Table:
    """Build a table showing aggregated stats by query size."""
    table = Table(
        title="Aggregated by Query Size - all times in ms (avg across all models)",
        show_header=True,
        header_style="bold cyan",
    )

    table.add_column("Query Size", style="yellow")
    table.add_column("CPU Mean", justify="right")
    table.add_column("CPU P50", justify="right")
    table.add_column("CPU P95", justify="right")
    if gpu_available:
        table.add_column("GPU Mean", justify="right")
        table.add_column("GPU P50", justify="right")
        table.add_column("GPU P95", justify="right")
        table.add_column("Avg Speedup", justify="right", style="bold green")

    for agg in aggregations:
        if gpu_available:
            speedup_str = f"{agg.speedup:.1f}x" if agg.speedup else "N/A"
            table.add_row(
                agg.query_length,
                f"{agg.cpu_mean_ms:.2f}",
                f"{agg.cpu_p50_ms:.2f}",
                f"{agg.cpu_p95_ms:.2f}",
                f"{agg.gpu_mean_ms:.2f}" if agg.gpu_mean_ms else "N/A",
                f"{agg.gpu_p50_ms:.2f}" if agg.gpu_p50_ms else "N/A",
                f"{agg.gpu_p95_ms:.2f}" if agg.gpu_p95_ms else "N/A",
                speedup_str,
            )
        else:
            table.add_row(
                agg.query_length,
                f"{agg.cpu_mean_ms:.2f}",
                f"{agg.cpu_p50_ms:.2f}",
                f"{agg.cpu_p95_ms:.2f}",
            )

    return table


def build_model_aggregation_table(
    aggregations: list[ModelSizeAggregation], gpu_available: bool
) -> Table:
    """Build a table showing aggregated stats by model size."""
    table = Table(
        title="Aggregated by Model Size - all times in ms (avg across all query lengths)",
        show_header=True,
        header_style="bold cyan",
    )

    table.add_column("Model Size", style="green")
    table.add_column("CPU Mean", justify="right")
    table.add_column("CPU P50", justify="right")
    table.add_column("CPU P95", justify="right")
    if gpu_available:
        table.add_column("GPU Mean", justify="right")
        table.add_column("GPU P50", justify="right")
        table.add_column("GPU P95", justify="right")
        table.add_column("Avg Speedup", justify="right", style="bold green")

    for agg in aggregations:
        if gpu_available:
            speedup_str = f"{agg.speedup:.1f}x" if agg.speedup else "N/A"
            table.add_row(
                agg.model_size,
                f"{agg.cpu_mean_ms:.2f}",
                f"{agg.cpu_p50_ms:.2f}",
                f"{agg.cpu_p95_ms:.2f}",
                f"{agg.gpu_mean_ms:.2f}" if agg.gpu_mean_ms else "N/A",
                f"{agg.gpu_p50_ms:.2f}" if agg.gpu_p50_ms else "N/A",
                f"{agg.gpu_p95_ms:.2f}" if agg.gpu_p95_ms else "N/A",
                speedup_str,
            )
        else:
            table.add_row(
                agg.model_size,
                f"{agg.cpu_mean_ms:.2f}",
                f"{agg.cpu_p50_ms:.2f}",
                f"{agg.cpu_p95_ms:.2f}",
            )

    return table


def build_model_info_table(comparison_rows: list[ComparisonRow]) -> Table:
    """Build a table showing model metadata (one row per model)."""
    table = Table(
        title="Model Information",
        show_header=True,
        header_style="bold blue",
    )

    table.add_column("Model", style="cyan", no_wrap=True)
    table.add_column("Size Label", style="green")
    table.add_column("Parameters", justify="right")
    table.add_column("Embed Dim", justify="right")
    table.add_column("Max Seq Len", justify="right")
    table.add_column("Load Time", justify="right")
    table.add_column("Type", style="dim")

    # Dedupe by model name (since we have one row per model+query)
    seen_models: set[str] = set()
    for row in comparison_rows:
        if row.model_name in seen_models:
            continue
        seen_models.add(row.model_name)

        model_short = row.model_name.split("/")[-1]
        dim_str = str(row.embedding_dim) if row.embedding_dim else "?"
        max_seq_str = str(row.max_seq_length) if row.max_seq_length else "?"
        model_type = row.model_type or "?"

        table.add_row(
            model_short,
            row.model_size,
            row.params_str,
            dim_str,
            max_seq_str,
            row.load_time_str,
            model_type,
        )

    return table


def print_tables(
    results: BenchmarkResults, console: Console, dry_run: bool = False
) -> None:
    """Print all benchmark tables."""
    comparison_rows = build_comparison_data(results)

    # Always show model info table
    console.print()
    model_info_table = build_model_info_table(comparison_rows)
    console.print(model_info_table)

    # Skip timing tables for dry run
    if dry_run:
        console.print("\n[yellow]Dry run complete - no timing data collected[/yellow]")
        return

    query_aggs = build_query_aggregations(comparison_rows)
    model_aggs = build_model_aggregations(comparison_rows)

    # Full comparison table
    console.print()
    comparison_table = build_comparison_table(comparison_rows, results.gpu_available)
    console.print(comparison_table)

    # Aggregation by query size
    console.print()
    query_agg_table = build_aggregation_table(query_aggs, results.gpu_available)
    console.print(query_agg_table)

    # Aggregation by model size
    console.print()
    model_agg_table = build_model_aggregation_table(model_aggs, results.gpu_available)
    console.print(model_agg_table)


def export_json(results: BenchmarkResults, path: Path) -> None:
    """Export results to JSON with comparison format."""
    comparison_rows = build_comparison_data(results)
    query_aggs = build_query_aggregations(comparison_rows)
    model_aggs = build_model_aggregations(comparison_rows)

    hw = results.hardware
    hardware_info = {}
    if hw:
        hardware_info = {
            "cpu_model": hw.cpu_model,
            "cpu_cores": hw.cpu_cores,
            "cpu_threads": hw.cpu_threads,
            "ram_gb": round(hw.ram_gb, 1),
            "gpu_model": hw.gpu_model,
            "gpu_memory_gb": round(hw.gpu_memory_gb, 1) if hw.gpu_memory_gb else None,
            "cuda_version": hw.cuda_version,
            "torch_version": hw.torch_version,
        }

    data = {
        "run_timestamp": results.run_timestamp.isoformat(),
        "total_runtime_seconds": round(results.total_runtime_seconds, 1),
        "hardware": hardware_info,
        "config": {
            "iterations": results.iterations,
            "warmup": results.warmup,
            "gpu_available": results.gpu_available,
        },
        "comparison": [
            {
                "model": row.model_name,
                "size": row.model_size,
                "params": row.num_parameters,
                "params_str": row.params_str,
                "embedding_dim": row.embedding_dim,
                "max_seq_length": row.max_seq_length,
                "model_type": row.model_type,
                "cpu_load_time_ms": row.cpu_load_time_ms,
                "gpu_load_time_ms": row.gpu_load_time_ms,
                "query_length": row.query_length,
                "cpu_mean_ms": row.cpu_mean_ms,
                "cpu_p50_ms": row.cpu_p50_ms,
                "cpu_p95_ms": row.cpu_p95_ms,
                "cpu_p99_ms": row.cpu_p99_ms,
                "gpu_mean_ms": row.gpu_mean_ms,
                "gpu_p50_ms": row.gpu_p50_ms,
                "gpu_p95_ms": row.gpu_p95_ms,
                "gpu_p99_ms": row.gpu_p99_ms,
                "speedup": row.speedup,
            }
            for row in comparison_rows
        ],
        "aggregations_by_query_size": [
            {
                "query_length": agg.query_length,
                "cpu_mean_ms": agg.cpu_mean_ms,
                "cpu_p50_ms": agg.cpu_p50_ms,
                "cpu_p95_ms": agg.cpu_p95_ms,
                "gpu_mean_ms": agg.gpu_mean_ms,
                "gpu_p50_ms": agg.gpu_p50_ms,
                "gpu_p95_ms": agg.gpu_p95_ms,
                "avg_speedup": agg.speedup,
            }
            for agg in query_aggs
        ],
        "aggregations_by_model_size": [
            {
                "model_size": agg.model_size,
                "cpu_mean_ms": agg.cpu_mean_ms,
                "cpu_p50_ms": agg.cpu_p50_ms,
                "cpu_p95_ms": agg.cpu_p95_ms,
                "gpu_mean_ms": agg.gpu_mean_ms,
                "gpu_p50_ms": agg.gpu_p50_ms,
                "gpu_p95_ms": agg.gpu_p95_ms,
                "avg_speedup": agg.speedup,
            }
            for agg in model_aggs
        ],
    }
    path.write_text(json.dumps(data, indent=2))


def export_csv(results: BenchmarkResults, path: Path) -> None:
    """Export results to CSV with comparison format."""
    comparison_rows = build_comparison_data(results)

    lines = [
        "model,size,params,embedding_dim,max_seq_length,cpu_load_time_ms,gpu_load_time_ms,"
        "query_length,cpu_mean_ms,cpu_p50_ms,cpu_p95_ms,cpu_p99_ms,"
        "gpu_mean_ms,gpu_p50_ms,gpu_p95_ms,gpu_p99_ms,speedup"
    ]
    for row in comparison_rows:
        params = str(row.num_parameters) if row.num_parameters else ""
        embed_dim = str(row.embedding_dim) if row.embedding_dim else ""
        max_seq = str(row.max_seq_length) if row.max_seq_length else ""
        cpu_load = f"{row.cpu_load_time_ms:.1f}" if row.cpu_load_time_ms else ""
        gpu_load = f"{row.gpu_load_time_ms:.1f}" if row.gpu_load_time_ms else ""
        gpu_mean = f"{row.gpu_mean_ms:.4f}" if row.gpu_mean_ms else ""
        gpu_p50 = f"{row.gpu_p50_ms:.4f}" if row.gpu_p50_ms else ""
        gpu_p95 = f"{row.gpu_p95_ms:.4f}" if row.gpu_p95_ms else ""
        gpu_p99 = f"{row.gpu_p99_ms:.4f}" if row.gpu_p99_ms else ""
        speedup = f"{row.speedup:.2f}" if row.speedup else ""
        lines.append(
            f"{row.model_name},{row.model_size},{params},{embed_dim},{max_seq},"
            f"{cpu_load},{gpu_load},{row.query_length},"
            f"{row.cpu_mean_ms:.4f},{row.cpu_p50_ms:.4f},{row.cpu_p95_ms:.4f},{row.cpu_p99_ms:.4f},"
            f"{gpu_mean},{gpu_p50},{gpu_p95},{gpu_p99},{speedup}"
        )
    path.write_text("\n".join(lines))


def export_markdown(results: BenchmarkResults, path: Path) -> None:
    """Export results to Markdown with comparison format."""
    comparison_rows = build_comparison_data(results)
    query_aggs = build_query_aggregations(comparison_rows)
    model_aggs = build_model_aggregations(comparison_rows)

    # Format timestamp
    run_time = results.run_timestamp.strftime("%Y-%m-%d %H:%M:%S UTC")
    minutes, seconds = divmod(results.total_runtime_seconds, 60)
    runtime_str = (
        f"{int(minutes)}m {seconds:.1f}s" if minutes > 0 else f"{seconds:.1f}s"
    )

    lines = [
        "# HuggingFace Embedding Benchmark Results",
        "",
        f"**Run Date:** {run_time}  ",
        f"**Total Runtime:** {runtime_str}",
        "",
    ]

    # Hardware info
    hw = results.hardware
    if hw:
        lines.extend(
            [
                "## Hardware",
                "",
                f"- **CPU:** {hw.cpu_model}",
                f"- **CPU Cores/Threads:** {hw.cpu_cores}/{hw.cpu_threads}",
                f"- **RAM:** {hw.ram_gb:.1f} GB",
            ]
        )
        if hw.gpu_model:
            lines.extend(
                [
                    f"- **GPU:** {hw.gpu_model}",
                    f"- **GPU Memory:** {hw.gpu_memory_gb:.1f} GB",
                    f"- **CUDA:** {hw.cuda_version}",
                ]
            )
        lines.extend(
            [
                f"- **PyTorch:** {hw.torch_version}",
                "",
            ]
        )

    lines.extend(
        [
            "## Configuration",
            "",
            f"- **Iterations:** {results.iterations}",
            f"- **Warmup:** {results.warmup}",
            f"- **GPU Available:** {'Yes' if results.gpu_available else 'No'}",
            "",
            "## Models",
            "",
            "| Model | Size | Params | Dim | Max Seq | Load Time | Type |",
            "|-------|------|--------|-----|---------|-----------|------|",
        ]
    )

    # Dedupe models for the models table
    seen_models: set[str] = set()
    for row in comparison_rows:
        if row.model_name in seen_models:
            continue
        seen_models.add(row.model_name)
        model_short = row.model_name.split("/")[-1]
        dim_str = str(row.embedding_dim) if row.embedding_dim else "?"
        max_seq_str = str(row.max_seq_length) if row.max_seq_length else "?"
        model_type = row.model_type or "?"
        lines.append(
            f"| {model_short} | {row.model_size} | {row.params_str} | {dim_str} | "
            f"{max_seq_str} | {row.load_time_str} | {model_type} |"
        )

    lines.extend(
        [
            "",
            "## CPU vs GPU Comparison - all times in ms (per model + query)",
            "",
        ]
    )

    if results.gpu_available:
        lines.extend(
            [
                "| Model | Size | Params | Dim | Query | CPU Mean | CPU P50 | CPU P95 | GPU Mean | GPU P50 | GPU P95 | Speedup |",
                "|-------|------|--------|-----|-------|----------|---------|---------|----------|---------|---------|---------|",
            ]
        )
        for row in comparison_rows:
            model_short = row.model_name.split("/")[-1]
            dim_str = str(row.embedding_dim) if row.embedding_dim else "?"
            speedup_str = f"{row.speedup:.1f}x" if row.speedup else "N/A"
            gpu_mean = f"{row.gpu_mean_ms:.2f}" if row.gpu_mean_ms else "N/A"
            gpu_p50 = f"{row.gpu_p50_ms:.2f}" if row.gpu_p50_ms else "N/A"
            gpu_p95 = f"{row.gpu_p95_ms:.2f}" if row.gpu_p95_ms else "N/A"
            lines.append(
                f"| {model_short} | {row.model_size} | {row.params_str} | {dim_str} | {row.query_length} | "
                f"{row.cpu_mean_ms:.2f} | {row.cpu_p50_ms:.2f} | {row.cpu_p95_ms:.2f} | "
                f"{gpu_mean} | {gpu_p50} | {gpu_p95} | {speedup_str} |"
            )
    else:
        lines.extend(
            [
                "| Model | Size | Params | Dim | Query | CPU Mean | CPU P50 | CPU P95 |",
                "|-------|------|--------|-----|-------|----------|---------|---------|",
            ]
        )
        for row in comparison_rows:
            model_short = row.model_name.split("/")[-1]
            dim_str = str(row.embedding_dim) if row.embedding_dim else "?"
            lines.append(
                f"| {model_short} | {row.model_size} | {row.params_str} | {dim_str} | {row.query_length} | "
                f"{row.cpu_mean_ms:.2f} | {row.cpu_p50_ms:.2f} | {row.cpu_p95_ms:.2f} |"
            )

    # Aggregations section
    lines.extend(
        [
            "",
            "## Aggregated by Query Size - all times in ms (avg across all models)",
            "",
        ]
    )

    if results.gpu_available:
        lines.extend(
            [
                "| Query Size | CPU Mean | CPU P50 | CPU P95 | GPU Mean | GPU P50 | GPU P95 | Avg Speedup |",
                "|------------|----------|---------|---------|----------|---------|---------|-------------|",
            ]
        )
        for agg in query_aggs:
            speedup_str = f"{agg.speedup:.1f}x" if agg.speedup else "N/A"
            gpu_mean = f"{agg.gpu_mean_ms:.2f}" if agg.gpu_mean_ms else "N/A"
            gpu_p50 = f"{agg.gpu_p50_ms:.2f}" if agg.gpu_p50_ms else "N/A"
            gpu_p95 = f"{agg.gpu_p95_ms:.2f}" if agg.gpu_p95_ms else "N/A"
            lines.append(
                f"| {agg.query_length} | {agg.cpu_mean_ms:.2f} | {agg.cpu_p50_ms:.2f} | "
                f"{agg.cpu_p95_ms:.2f} | {gpu_mean} | {gpu_p50} | {gpu_p95} | {speedup_str} |"
            )
    else:
        lines.extend(
            [
                "| Query Size | CPU Mean | CPU P50 | CPU P95 |",
                "|------------|----------|---------|---------|",
            ]
        )
        for agg in query_aggs:
            lines.append(
                f"| {agg.query_length} | {agg.cpu_mean_ms:.2f} | {agg.cpu_p50_ms:.2f} | "
                f"{agg.cpu_p95_ms:.2f} |"
            )

    # Model size aggregations section
    lines.extend(
        [
            "",
            "## Aggregated by Model Size - all times in ms (avg across all query lengths)",
            "",
        ]
    )

    if results.gpu_available:
        lines.extend(
            [
                "| Model Size | CPU Mean | CPU P50 | CPU P95 | GPU Mean | GPU P50 | GPU P95 | Avg Speedup |",
                "|------------|----------|---------|---------|----------|---------|---------|-------------|",
            ]
        )
        for agg in model_aggs:
            speedup_str = f"{agg.speedup:.1f}x" if agg.speedup else "N/A"
            gpu_mean = f"{agg.gpu_mean_ms:.2f}" if agg.gpu_mean_ms else "N/A"
            gpu_p50 = f"{agg.gpu_p50_ms:.2f}" if agg.gpu_p50_ms else "N/A"
            gpu_p95 = f"{agg.gpu_p95_ms:.2f}" if agg.gpu_p95_ms else "N/A"
            lines.append(
                f"| {agg.model_size} | {agg.cpu_mean_ms:.2f} | {agg.cpu_p50_ms:.2f} | "
                f"{agg.cpu_p95_ms:.2f} | {gpu_mean} | {gpu_p50} | {gpu_p95} | {speedup_str} |"
            )
    else:
        lines.extend(
            [
                "| Model Size | CPU Mean | CPU P50 | CPU P95 |",
                "|------------|----------|---------|---------|",
            ]
        )
        for agg in model_aggs:
            lines.append(
                f"| {agg.model_size} | {agg.cpu_mean_ms:.2f} | {agg.cpu_p50_ms:.2f} | "
                f"{agg.cpu_p95_ms:.2f} |"
            )

    path.write_text("\n".join(lines))


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

app = typer.Typer(help="HuggingFace Embedding Benchmark: CPU vs GPU latency")


@app.command()
def main(
    iterations: Annotated[
        int | None,
        typer.Option("--iterations", "-n", help="Number of timed iterations"),
    ] = None,
    warmup: Annotated[
        int | None, typer.Option("--warmup", "-w", help="Number of warmup iterations")
    ] = None,
    fmt: Annotated[
        list[OutputFormat] | None,
        typer.Option("--fmt", "-f", help="Output formats (can specify multiple)"),
    ] = None,
    full: Annotated[
        bool,
        typer.Option("--full", help="Run full benchmark with 20 diverse models"),
    ] = False,
    dry_run: Annotated[
        bool,
        typer.Option(
            "--dry-run", help="Load models and show info without running timing"
        ),
    ] = False,
) -> None:
    """Run embedding benchmarks comparing CPU vs GPU latency."""
    console = Console()
    start_time = time.perf_counter()

    # Set defaults based on mode
    if iterations is None:
        iterations = 10 if full else 100
    if warmup is None:
        warmup = 3 if full else 10

    console.print("[bold]HuggingFace Embedding Benchmark[/bold]")
    console.print("Measuring single-query embedding latency for CPU vs GPU\n")

    # Run benchmarks
    results = run_benchmarks(
        iterations=iterations,
        warmup=warmup,
        console=console,
        full_mode=full,
        dry_run=dry_run,
    )

    # Record total runtime
    results.total_runtime_seconds = time.perf_counter() - start_time

    # Print comparison tables
    print_tables(results, console, dry_run=dry_run)

    # Export to requested formats (skip for dry run)
    if fmt:
        output_dir = Path("benchmarks/results")
        output_dir.mkdir(parents=True, exist_ok=True)

        for output_format in fmt:
            if output_format == OutputFormat.JSON:
                path = output_dir / "hf_embedding_bench.json"
                export_json(results, path)
                console.print(f"\n[green]Exported JSON to {path}[/green]")
            elif output_format == OutputFormat.CSV:
                path = output_dir / "hf_embedding_bench.csv"
                export_csv(results, path)
                console.print(f"[green]Exported CSV to {path}[/green]")
            elif output_format == OutputFormat.MD:
                path = output_dir / "hf_embedding_bench.md"
                export_markdown(results, path)
                console.print(f"[green]Exported Markdown to {path}[/green]")

    # Print total runtime
    minutes, seconds = divmod(results.total_runtime_seconds, 60)
    console.print(
        f"\n[bold green]Benchmark complete![/bold green] Total runtime: {int(minutes)}m {seconds:.1f}s"
    )


if __name__ == "__main__":
    app()
