# HuggingFace Embedding Benchmark Results

**Run Date:** 2025-11-27 17:40:17 UTC  
**Total Runtime:** 3m 31.1s

## Hardware

- **CPU:** Intel(R) Core(TM) Ultra 9 285K
- **CPU Cores/Threads:** 1/24
- **RAM:** 91.3 GB
- **GPU:** NVIDIA GeForce RTX 4080 SUPER
- **GPU Memory:** 15.6 GB
- **CUDA:** 12.8
- **PyTorch:** 2.9.1+cu128

## Configuration

- **Iterations:** 30
- **Warmup:** 10
- **GPU Available:** Yes

## Models

| Model | Size | Params | Dim | Max Seq | Load Time | Type |
|-------|------|--------|-----|---------|-----------|------|
| all-MiniLM-L6-v2 | tiny-minilm-l6 | 23M | 384 | 256 | 707ms | bert |
| paraphrase-MiniLM-L3-v2 | tiny-minilm-l3 | 17M | 384 | 128 | 610ms | bert |
| all-MiniLM-L12-v2 | small-minilm-l12 | 33M | 384 | 128 | 585ms | bert |
| bge-small-en-v1.5 | small-bge | 33M | 384 | 512 | 617ms | bert |
| all-mpnet-base-v2 | medium-mpnet | 109M | 768 | 384 | 760ms | mpnet |
| bge-base-en-v1.5 | medium-bge | 109M | 768 | 512 | 659ms | bert |
| gte-base | medium-gte | 109M | 768 | 512 | 664ms | bert |
| e5-base-v2 | medium-e5 | 109M | 768 | 512 | 609ms | bert |
| bge-large-en-v1.5 | large-bge | 335M | 1024 | 512 | 622ms | bert |
| gte-large | large-gte | 335M | 1024 | 512 | 753ms | bert |
| e5-large-v2 | large-e5 | 335M | 1024 | 512 | 665ms | bert |
| paraphrase-multilingual-MiniLM-L12-v2 | multi-minilm | 118M | 384 | 128 | 1.0s | bert |
| multilingual-e5-base | multi-e5 | 278M | 768 | 512 | 1.1s | xlm-roberta |
| jina-embeddings-v2-base-code | code-jina | 161M | 768 | 8192 | 1.1s | bert |
| jina-embeddings-v2-base-en | long-jina | 137M | 768 | 8192 | 1.5s | bert |
| nomic-embed-text-v1.5 | matryoshka-nomic | 137M | 768 | 8192 | 1.5s | nomic_bert |
| Splade_PP_en_v1 | sparse-small | 110M | 30522 | 512 | 365ms | bert |
| efficient-splade-VI-BT-large-query | sparse-large | 4M | 30522 | 512 | 225ms | bert |

## CPU vs GPU Comparison - all times in ms (per model + query)

| Model | Size | Params | Dim | Query | CPU Mean | CPU P50 | CPU P95 | GPU Mean | GPU P50 | GPU P95 | Speedup |
|-------|------|--------|-----|-------|----------|---------|---------|----------|---------|---------|---------|
| all-MiniLM-L6-v2 | tiny-minilm-l6 | 23M | 384 | short | 2.54 | 2.47 | 3.37 | 1.58 | 1.55 | 2.01 | 1.6x |
| all-MiniLM-L6-v2 | tiny-minilm-l6 | 23M | 384 | medium | 3.79 | 2.78 | 15.30 | 1.63 | 1.60 | 2.02 | 2.3x |
| all-MiniLM-L6-v2 | tiny-minilm-l6 | 23M | 384 | long | 4.44 | 4.16 | 6.72 | 1.71 | 1.72 | 1.77 | 2.6x |
| paraphrase-MiniLM-L3-v2 | tiny-minilm-l3 | 17M | 384 | short | 1.67 | 1.52 | 2.47 | 1.04 | 1.04 | 1.08 | 1.6x |
| paraphrase-MiniLM-L3-v2 | tiny-minilm-l3 | 17M | 384 | medium | 1.80 | 1.59 | 3.17 | 1.13 | 1.08 | 1.54 | 1.6x |
| paraphrase-MiniLM-L3-v2 | tiny-minilm-l3 | 17M | 384 | long | 30.16 | 2.43 | 207.78 | 1.17 | 1.17 | 1.22 | 25.7x |
| all-MiniLM-L12-v2 | small-minilm-l12 | 33M | 384 | short | 4.43 | 4.27 | 6.04 | 2.51 | 2.47 | 2.91 | 1.8x |
| all-MiniLM-L12-v2 | small-minilm-l12 | 33M | 384 | medium | 4.68 | 4.49 | 6.63 | 2.63 | 2.60 | 3.01 | 1.8x |
| all-MiniLM-L12-v2 | small-minilm-l12 | 33M | 384 | long | 10.11 | 7.26 | 39.04 | 2.70 | 2.65 | 3.17 | 3.8x |
| bge-small-en-v1.5 | small-bge | 33M | 384 | short | 19.89 | 4.30 | 216.30 | 2.51 | 2.47 | 2.91 | 7.9x |
| bge-small-en-v1.5 | small-bge | 33M | 384 | medium | 4.85 | 4.67 | 7.21 | 2.62 | 2.59 | 3.05 | 1.9x |
| bge-small-en-v1.5 | small-bge | 33M | 384 | long | 22.00 | 6.86 | 203.50 | 2.66 | 2.62 | 3.13 | 8.3x |
| all-mpnet-base-v2 | medium-mpnet | 109M | 768 | short | 12.81 | 9.66 | 42.67 | 2.89 | 2.86 | 3.28 | 4.4x |
| all-mpnet-base-v2 | medium-mpnet | 109M | 768 | medium | 12.22 | 10.07 | 36.30 | 2.85 | 2.84 | 2.90 | 4.3x |
| all-mpnet-base-v2 | medium-mpnet | 109M | 768 | long | 475.72 | 22.17 | 1788.65 | 3.19 | 3.11 | 4.22 | 149.0x |
| bge-base-en-v1.5 | medium-bge | 109M | 768 | short | 88.98 | 9.50 | 713.50 | 2.58 | 2.55 | 2.98 | 34.5x |
| bge-base-en-v1.5 | medium-bge | 109M | 768 | medium | 16.51 | 11.61 | 65.41 | 2.59 | 2.54 | 3.10 | 6.4x |
| bge-base-en-v1.5 | medium-bge | 109M | 768 | long | 30.73 | 16.18 | 216.16 | 2.64 | 2.59 | 3.16 | 11.7x |
| gte-base | medium-gte | 109M | 768 | short | 12.47 | 10.01 | 39.94 | 2.58 | 2.55 | 3.10 | 4.8x |
| gte-base | medium-gte | 109M | 768 | medium | 84.74 | 10.65 | 805.78 | 2.61 | 2.55 | 3.07 | 32.5x |
| gte-base | medium-gte | 109M | 768 | long | 101.25 | 16.94 | 1120.11 | 2.70 | 2.64 | 3.25 | 37.5x |
| e5-base-v2 | medium-e5 | 109M | 768 | short | 10.50 | 9.51 | 20.15 | 2.66 | 2.60 | 3.51 | 4.0x |
| e5-base-v2 | medium-e5 | 109M | 768 | medium | 13.30 | 10.55 | 43.58 | 2.66 | 2.61 | 3.28 | 5.0x |
| e5-base-v2 | medium-e5 | 109M | 768 | long | 65.51 | 15.87 | 710.74 | 2.72 | 2.67 | 3.28 | 24.1x |
| bge-large-en-v1.5 | large-bge | 335M | 1024 | short | 318.14 | 35.89 | 1427.04 | 4.75 | 4.65 | 5.32 | 67.0x |
| bge-large-en-v1.5 | large-bge | 335M | 1024 | medium | 340.77 | 39.49 | 1593.67 | 4.72 | 4.65 | 5.22 | 72.2x |
| bge-large-en-v1.5 | large-bge | 335M | 1024 | long | 182.47 | 55.39 | 1656.99 | 6.20 | 6.14 | 6.90 | 29.4x |
| gte-large | large-gte | 335M | 1024 | short | 152.80 | 36.25 | 947.31 | 4.69 | 4.65 | 5.19 | 32.5x |
| gte-large | large-gte | 335M | 1024 | medium | 110.21 | 36.75 | 777.70 | 4.78 | 4.71 | 5.33 | 23.1x |
| gte-large | large-gte | 335M | 1024 | long | 392.18 | 60.12 | 2475.54 | 6.22 | 6.15 | 6.84 | 63.1x |
| e5-large-v2 | large-e5 | 335M | 1024 | short | 147.76 | 37.84 | 1253.54 | 4.73 | 4.67 | 5.26 | 31.3x |
| e5-large-v2 | large-e5 | 335M | 1024 | medium | 440.47 | 51.16 | 1595.90 | 4.76 | 4.70 | 5.32 | 92.6x |
| e5-large-v2 | large-e5 | 335M | 1024 | long | 83.86 | 61.42 | 294.52 | 6.20 | 6.17 | 6.58 | 13.5x |
| paraphrase-multilingual-MiniLM-L12-v2 | multi-minilm | 118M | 384 | short | 5.59 | 5.38 | 7.11 | 2.52 | 2.48 | 3.01 | 2.2x |
| paraphrase-multilingual-MiniLM-L12-v2 | multi-minilm | 118M | 384 | medium | 6.16 | 5.92 | 8.27 | 2.75 | 2.65 | 3.73 | 2.2x |
| paraphrase-multilingual-MiniLM-L12-v2 | multi-minilm | 118M | 384 | long | 10.36 | 7.30 | 44.25 | 2.69 | 2.64 | 3.31 | 3.9x |
| multilingual-e5-base | multi-e5 | 278M | 768 | short | 13.95 | 9.69 | 58.45 | 2.70 | 2.69 | 3.09 | 5.2x |
| multilingual-e5-base | multi-e5 | 278M | 768 | medium | 11.29 | 10.73 | 15.88 | 2.71 | 2.66 | 3.38 | 4.2x |
| multilingual-e5-base | multi-e5 | 278M | 768 | long | 18.24 | 15.99 | 43.18 | 2.78 | 2.78 | 2.93 | 6.6x |
| jina-embeddings-v2-base-code | code-jina | 161M | 768 | short | 23.73 | 12.66 | 116.45 | 3.26 | 3.21 | 3.79 | 7.3x |
| jina-embeddings-v2-base-code | code-jina | 161M | 768 | medium | 43.76 | 14.32 | 412.31 | 3.30 | 3.25 | 3.69 | 13.3x |
| jina-embeddings-v2-base-code | code-jina | 161M | 768 | long | 22.76 | 19.57 | 53.84 | 3.34 | 3.30 | 3.79 | 6.8x |
| jina-embeddings-v2-base-en | long-jina | 137M | 768 | short | 39.13 | 13.73 | 351.57 | 2.91 | 2.85 | 3.33 | 13.5x |
| jina-embeddings-v2-base-en | long-jina | 137M | 768 | medium | 60.32 | 13.46 | 612.75 | 2.92 | 2.89 | 3.27 | 20.6x |
| jina-embeddings-v2-base-en | long-jina | 137M | 768 | long | 26.90 | 19.80 | 73.07 | 3.05 | 3.02 | 3.48 | 8.8x |
| nomic-embed-text-v1.5 | matryoshka-nomic | 137M | 768 | short | 13.48 | 11.88 | 24.19 | 3.35 | 3.31 | 3.63 | 4.0x |
| nomic-embed-text-v1.5 | matryoshka-nomic | 137M | 768 | medium | 69.01 | 15.95 | 667.47 | 3.46 | 3.42 | 3.77 | 19.9x |
| nomic-embed-text-v1.5 | matryoshka-nomic | 137M | 768 | long | 110.59 | 21.53 | 1077.92 | 3.48 | 3.40 | 4.27 | 31.8x |
| Splade_PP_en_v1 | sparse-small | 110M | 30522 | short | 10.91 | 10.53 | 13.58 | 1.98 | 1.94 | 2.57 | 5.5x |
| Splade_PP_en_v1 | sparse-small | 110M | 30522 | medium | 100.72 | 13.01 | 841.27 | 2.10 | 2.02 | 2.94 | 48.0x |
| Splade_PP_en_v1 | sparse-small | 110M | 30522 | long | 23.65 | 21.13 | 40.14 | 2.10 | 2.06 | 2.55 | 11.3x |
| efficient-splade-VI-BT-large-query | sparse-large | 4M | 30522 | short | 0.61 | 0.58 | 0.76 | 0.57 | 0.57 | 0.66 | 1.1x |
| efficient-splade-VI-BT-large-query | sparse-large | 4M | 30522 | medium | 0.73 | 0.69 | 0.90 | 0.60 | 0.59 | 0.75 | 1.2x |
| efficient-splade-VI-BT-large-query | sparse-large | 4M | 30522 | long | 1.85 | 1.73 | 3.10 | 0.68 | 0.68 | 0.70 | 2.7x |

## Aggregated by Query Size - all times in ms (avg across all models)

| Query Size | CPU Mean | CPU P50 | CPU P95 | GPU Mean | GPU P50 | GPU P95 | Avg Speedup |
|------------|----------|---------|---------|----------|---------|---------|-------------|
| short | 48.86 | 12.54 | 291.36 | 2.77 | 2.73 | 3.20 | 17.7x |
| medium | 73.63 | 14.33 | 417.19 | 2.82 | 2.78 | 3.30 | 26.1x |
| long | 89.60 | 20.88 | 558.62 | 3.12 | 3.08 | 3.59 | 28.7x |

## Aggregated by Model Size - all times in ms (avg across all query lengths)

| Model Size | CPU Mean | CPU P50 | CPU P95 | GPU Mean | GPU P50 | GPU P95 | Avg Speedup |
|------------|----------|---------|---------|----------|---------|---------|-------------|
| tiny-minilm-l6 | 3.59 | 3.14 | 8.46 | 1.64 | 1.62 | 1.93 | 2.2x |
| tiny-minilm-l3 | 11.21 | 1.85 | 71.14 | 1.11 | 1.10 | 1.28 | 10.1x |
| small-minilm-l12 | 6.41 | 5.34 | 17.24 | 2.61 | 2.57 | 3.03 | 2.5x |
| small-bge | 15.58 | 5.27 | 142.34 | 2.60 | 2.56 | 3.03 | 6.0x |
| medium-mpnet | 166.91 | 13.97 | 622.54 | 2.98 | 2.94 | 3.47 | 56.1x |
| medium-bge | 45.41 | 12.43 | 331.69 | 2.60 | 2.56 | 3.08 | 17.5x |
| medium-gte | 66.15 | 12.53 | 655.27 | 2.63 | 2.58 | 3.14 | 25.2x |
| medium-e5 | 29.77 | 11.98 | 258.15 | 2.68 | 2.63 | 3.36 | 11.1x |
| large-bge | 280.46 | 43.59 | 1559.23 | 5.22 | 5.15 | 5.82 | 53.7x |
| large-gte | 218.40 | 44.38 | 1400.19 | 5.23 | 5.17 | 5.79 | 41.8x |
| large-e5 | 224.03 | 50.14 | 1047.99 | 5.23 | 5.18 | 5.72 | 42.8x |
| multi-minilm | 7.37 | 6.20 | 19.88 | 2.65 | 2.59 | 3.35 | 2.8x |
| multi-e5 | 14.49 | 12.14 | 39.17 | 2.73 | 2.71 | 3.13 | 5.3x |
| code-jina | 30.08 | 15.52 | 194.20 | 3.30 | 3.25 | 3.76 | 9.1x |
| long-jina | 42.12 | 15.66 | 345.80 | 2.96 | 2.92 | 3.36 | 14.2x |
| matryoshka-nomic | 64.36 | 16.45 | 589.86 | 3.43 | 3.38 | 3.89 | 18.8x |
| sparse-small | 45.09 | 14.89 | 298.33 | 2.06 | 2.01 | 2.68 | 21.9x |
| sparse-large | 1.06 | 1.00 | 1.59 | 0.62 | 0.61 | 0.70 | 1.7x |