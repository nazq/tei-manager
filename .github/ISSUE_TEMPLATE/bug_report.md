---
name: Bug Report
about: Report a bug or unexpected behavior
title: '[BUG] '
labels: bug
assignees: ''
---

## Description

A clear description of the bug.

## Steps to Reproduce

1. Start TEI Manager with config: ...
2. Create instance: `curl -X POST ...`
3. Send request: ...
4. See error

## Expected Behavior

What you expected to happen.

## Actual Behavior

What actually happened. Include error messages.

## Environment

- **TEI Manager version**: (e.g., 0.6.0)
- **Docker image tag**: (e.g., `0.6.0-tei-1.8.3-ada`)
- **GPU**: (e.g., RTX 4090, H100)
- **CUDA version**: (output of `nvidia-smi`)
- **OS**: (e.g., Ubuntu 22.04)

## Logs

<details>
<summary>TEI Manager logs</summary>

```
Paste relevant logs here
```

</details>

<details>
<summary>Instance logs (if applicable)</summary>

```
curl http://localhost:9000/instances/{name}/logs
```

</details>

## Additional Context

Any other context about the problem.
