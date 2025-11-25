# /// script
# requires-python = ">=3.13"
# dependencies = [
#     "torch",
#     "transformers",
#     "numpy",
# ]
# ///
"""Raw GPU saturation test - Direct HuggingFace Transformers, no TEI"""
import torch
import time
import sys
import json
import urllib.request
import ssl
from transformers import AutoTokenizer, AutoModel
import numpy as np

def get_model_from_tei_manager(instance_name, tei_endpoint, cert_path, key_path):
    """Query TEI Manager API to get model ID for an instance"""
    try:
        # Parse endpoint to get base URL (swap gRPC port 9001 to API port 9000)
        import re
        base_url = re.sub(r':9001\b', ':9000', tei_endpoint)
        url = f"{base_url}/instances"

        # Determine if using HTTPS
        use_https = base_url.startswith("https://")

        if use_https:
            # Create SSL context that accepts self-signed certs
            ssl_context = ssl.create_default_context()
            ssl_context.check_hostname = False
            ssl_context.verify_mode = ssl.CERT_NONE

            # Load client certificate if provided and files exist
            if cert_path and key_path:
                import os
                if os.path.exists(cert_path) and os.path.exists(key_path):
                    ssl_context.load_cert_chain(cert_path, key_path)
        else:
            ssl_context = None

        req = urllib.request.Request(url)
        with urllib.request.urlopen(req, context=ssl_context) as response:
            instances = json.loads(response.read().decode())

        # Find the instance by name
        for instance in instances:
            if instance["name"] == instance_name:
                model_id = instance["model_id"]
                print(f"✓ Found instance '{instance_name}' → {model_id}")
                return model_id

        print(f"⚠ Instance '{instance_name}' not found in TEI Manager")
        return None
    except Exception as e:
        print(f"⚠ Failed to query TEI Manager: {e}")
        return None

def load_model(model_name_or_id, tei_endpoint=None, cert_path=None, key_path=None):
    """Load model directly to GPU"""
    # If it looks like a HuggingFace model ID (contains /), use it directly
    if "/" in model_name_or_id:
        model_id = model_name_or_id
        print(f"Loading {model_id}...")
    else:
        # Query TEI Manager if endpoint provided
        model_id = None
        if tei_endpoint:
            model_id = get_model_from_tei_manager(model_name_or_id, tei_endpoint, cert_path, key_path)

        if not model_id:
            print(f"✗ Could not resolve model for '{model_name_or_id}'")
            print(f"   Use a full HuggingFace model ID (e.g., BAAI/bge-small-en-v1.5)")
            print(f"   or ensure the instance exists in TEI Manager")
            sys.exit(1)

        print(f"Loading {model_id}...")

    tokenizer = AutoTokenizer.from_pretrained(model_id)
    model = AutoModel.from_pretrained(model_id, torch_dtype=torch.float16).cuda()
    model.eval()
    print(f"✓ Model loaded on GPU: {torch.cuda.get_device_name(0)}")
    return tokenizer, model, model_id

def embed_batch(texts, tokenizer, model):
    """Embed a batch of texts"""
    inputs = tokenizer(texts, padding=True, truncation=True, max_length=512, return_tensors="pt")
    inputs = {k: v.cuda() for k, v in inputs.items()}

    with torch.no_grad():
        outputs = model(**inputs)
        embeddings = outputs.last_hidden_state.mean(dim=1)

    return embeddings.cpu().numpy()

def test_batched(num_texts, batch_size, tokenizer, model, model_id):
    """Test embedding performance with batching"""
    texts = [f"This is test document number {i} for GPU saturation testing with transformers." for i in range(num_texts)]

    print(f"\n{'='*60}")
    print(f"Model:   {model_id}")
    print(f"Testing: {num_texts:,} texts in batches of {batch_size:,}")
    print(f"{'='*60}")

    # Warmup
    warmup_size = min(100, batch_size)
    _ = embed_batch(texts[:warmup_size], tokenizer, model)
    torch.cuda.synchronize()

    torch.cuda.empty_cache()

    start = time.time()
    all_embeddings = []

    try:
        # Process in batches
        for i in range(0, num_texts, batch_size):
            batch_texts = texts[i:i+batch_size]
            embeddings = embed_batch(batch_texts, tokenizer, model)
            all_embeddings.append(embeddings)

            # Show progress every 10 batches
            batch_num = (i // batch_size) + 1
            total_batches = (num_texts + batch_size - 1) // batch_size
            if batch_num % 10 == 0 or batch_num == total_batches:
                elapsed = time.time() - start
                processed = min(i + batch_size, num_texts)
                current_throughput = processed / elapsed
                print(f"  Batch {batch_num}/{total_batches}: {processed:,}/{num_texts:,} texts ({current_throughput:,.0f} emb/sec)")

        torch.cuda.synchronize()
        duration = time.time() - start

        all_embeddings = np.vstack(all_embeddings)
        throughput = num_texts / duration

        print(f"\n✓ SUCCESS")
        print(f"  Duration:    {duration:.2f}s")
        print(f"  Throughput:  {throughput:,.0f} emb/sec")
        print(f"  Output:      {all_embeddings.shape}")

        return True, throughput

    except torch.cuda.OutOfMemoryError:
        torch.cuda.empty_cache()
        print(f"\n✗ OOM - GPU memory exhausted at batch {batch_num}")
        return False, 0
    except Exception as e:
        print(f"\n✗ ERROR: {e}")
        return False, 0

def main():
    if len(sys.argv) < 3:
        print("Usage: raw-gpu-test.py <model> <num_texts> [batch_size] [--tei-endpoint ENDPOINT] [--cert CERT] [--key KEY]")
        print("")
        print("Arguments:")
        print("  model          - Instance name (queries TEI Manager) or HuggingFace model ID")
        print("  num_texts      - Total number of texts to embed")
        print("  batch_size     - Batch size for processing (default: num_texts, process in one batch)")
        print("  --tei-endpoint - TEI Manager endpoint (default: $GRPC_ENDPOINT)")
        print("  --cert         - Client certificate path (default: $CERT_PATH)")
        print("  --key          - Client key path (default: $KEY_PATH)")
        print("")
        print("Examples:")
        print("  # Query TEI Manager for instance model (uses env vars)")
        print("  raw-gpu-test.py test-small 50000 1000")
        print("")
        print("  # Override endpoint and certs")
        print("  raw-gpu-test.py test-small 50000 1000 \\")
        print("    --tei-endpoint https://localhost:9001 \\")
        print("    --cert certs/client.pem --key certs/client-key.pem")
        print("")
        print("  # Use full HuggingFace model ID directly")
        print("  raw-gpu-test.py BAAI/bge-small-en-v1.5 50000 1000")
        sys.exit(1)

    # Parse arguments
    model_name = sys.argv[1]
    num_texts = int(sys.argv[2])

    # Parse optional arguments (with env var defaults)
    import os
    batch_size = None
    tei_endpoint = os.getenv("TEI_API_ENDPOINT") or os.getenv("GRPC_ENDPOINT") or "http://localhost:9000"
    cert_path = os.getenv("TEI_CERT") or os.getenv("CERT_PATH") or None
    key_path = os.getenv("TEI_KEY") or os.getenv("KEY_PATH") or None

    i = 3
    while i < len(sys.argv):
        arg = sys.argv[i]
        if arg == "--tei-endpoint" and i + 1 < len(sys.argv):
            tei_endpoint = sys.argv[i + 1]
            i += 2
        elif arg == "--cert" and i + 1 < len(sys.argv):
            cert_path = sys.argv[i + 1]
            i += 2
        elif arg == "--key" and i + 1 < len(sys.argv):
            key_path = sys.argv[i + 1]
            i += 2
        elif batch_size is None and arg.isdigit():
            batch_size = int(arg)
            i += 1
        else:
            i += 1

    if batch_size is None:
        batch_size = num_texts

    tokenizer, model, model_id = load_model(model_name, tei_endpoint, cert_path, key_path)
    success, throughput = test_batched(num_texts, batch_size, tokenizer, model, model_id)

    if success:
        print(f"\n{'='*60}")
        print(f"RESULT: {throughput:,.0f} embeddings/second")
        print(f"{'='*60}")

if __name__ == "__main__":
    main()
