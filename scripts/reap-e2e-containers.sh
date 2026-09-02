#!/bin/bash
# Remove leaked e2e test containers (testcontainers cleans up on Drop, but a
# killed test process leaks its containers). Reaps containers labeled
# tei-manager.e2e=true older than AGE_MINUTES (default 60).
set -euo pipefail
AGE_MINUTES="${1:-60}"
CUTOFF=$(date -d "${AGE_MINUTES} minutes ago" +%s)
REAPED=0
for id in $(docker ps -q --filter label=tei-manager.e2e=true); do
    created=$(docker inspect "$id" --format '{{.Created}}')
    if [ "$(date -d "$created" +%s)" -lt "$CUTOFF" ]; then
        docker rm -f "$id" >/dev/null && REAPED=$((REAPED+1))
    fi
done
echo "Reaped $REAPED leaked e2e container(s) older than ${AGE_MINUTES}m"
