# h2 transport A/B — 2026-08-26, RTX 5090, bge-m3 fp16, TEI 1.9.2 (120- image)

Knobs are env-driven in this branch only (TEI_MUX_H2_WINDOW, TEI_MUX_SERVER_H2_WINDOW, TEI_MUX_CHANNELS).
Conclusion: all variants within ±5% noise; the model is GPU-bound on this card. Not shipped.

```
== base (image ) instance=running
  [short texts]
  arrow    b=1000  ok=40000  fail=0     4.31s      9281/s
  arrow    b=1000  ok=40000  fail=0     3.51s     11407/s
  standard b=64    ok=20000  fail=0     2.88s      6940/s
  [~200-word texts]
  arrow    b=500   ok=8000   fail=0     9.28s       862/s
  arrow    b=500   ok=8000   fail=0     9.03s       886/s
== exp0 (exp ) instance=running
  [short texts]
  arrow    b=1000  ok=40000  fail=0     4.63s      8647/s
  arrow    b=1000  ok=40000  fail=0     3.70s     10819/s
  standard b=64    ok=20000  fail=0     2.82s      7098/s
  [~200-word texts]
  arrow    b=500   ok=8000   fail=0     9.30s       860/s
  arrow    b=500   ok=8000   fail=0     9.07s       882/s
== exp-w8 (exp TEI_MUX_H2_WINDOW=8388608 TEI_MUX_SERVER_H2_WINDOW=8388608) instance=running
  [short texts]
  arrow    b=1000  ok=40000  fail=0     4.36s      9180/s
  arrow    b=1000  ok=40000  fail=0     3.94s     10159/s
  standard b=64    ok=20000  fail=0     2.98s      6720/s
  [~200-word texts]
  arrow    b=500   ok=8000   fail=0     9.32s       858/s
  arrow    b=500   ok=8000   fail=0     9.10s       879/s
== exp-c4 (exp TEI_MUX_CHANNELS=4) instance=running
  [short texts]
  arrow    b=1000  ok=40000  fail=0     4.08s      9809/s
  arrow    b=1000  ok=40000  fail=0     3.56s     11237/s
  standard b=64    ok=20000  fail=0     3.02s      6630/s
  [~200-word texts]
  arrow    b=500   ok=8000   fail=0     9.35s       855/s
  arrow    b=500   ok=8000   fail=0     9.08s       881/s
== exp-w8c4 (exp TEI_MUX_CHANNELS=4 TEI_MUX_H2_WINDOW=8388608 TEI_MUX_SERVER_H2_WINDOW=8388608) instance=running
  [short texts]
  arrow    b=1000  ok=40000  fail=0     4.16s      9610/s
  arrow    b=1000  ok=40000  fail=0     3.73s     10720/s
  standard b=64    ok=20000  fail=0     2.87s      6973/s
  [~200-word texts]
  arrow    b=500   ok=8000   fail=0     9.33s       857/s
  arrow    b=500   ok=8000   fail=0     9.16s       873/s
== base-mbt64k (image ) instance=running
  [short texts]
  arrow    b=1000  ok=40000  fail=0     4.29s      9323/s
  arrow    b=1000  ok=40000  fail=0     3.51s     11405/s
  standard b=64    ok=20000  fail=0     2.84s      7033/s
  [~200-word texts]
  arrow    b=500   ok=8000   fail=0     9.32s       858/s
  arrow    b=500   ok=8000   fail=0     9.13s       876/s
```
