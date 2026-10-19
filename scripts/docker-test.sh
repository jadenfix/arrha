#!/usr/bin/env bash
# Build and exercise the real four-validator Compose network.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT="aether-compose-e2e-${$}"
COMPOSE=(docker compose --project-name "$PROJECT" -f "$ROOT/docker-compose.test.yml")

cleanup() {
  "${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

"${COMPOSE[@]}" up --build --abort-on-container-exit --exit-code-from test-runner test-runner
