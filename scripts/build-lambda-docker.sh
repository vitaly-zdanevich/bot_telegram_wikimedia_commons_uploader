#!/usr/bin/env bash
set -euo pipefail

# Builds the Lambda zip WITH HEIC support inside an arm64 Amazon Linux 2023 container
# that compiles libde265 + libheif from source and links the bot with --features heic.
# Requires Docker with arm64 support (buildx + qemu on non-arm64 hosts).
#
# The Dockerfile's final stage exports build/lambda.zip via BuildKit local output.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v docker >/dev/null 2>&1; then
  echo "Docker is required for the HEIC build. Install Docker or run: HEIC=0 ./scripts/build-lambda.sh" >&2
  exit 1
fi

mkdir -p "$ROOT_DIR/build"
DOCKER_BUILDKIT=1 docker buildx build \
  --platform linux/arm64 \
  -f "$ROOT_DIR/Dockerfile" \
  -o "type=local,dest=$ROOT_DIR/build" \
  "$ROOT_DIR"

if [[ ! -f "$ROOT_DIR/build/lambda.zip" ]]; then
  echo "Docker build did not produce build/lambda.zip" >&2
  exit 1
fi
printf 'Wrote %s (%.1f MB)\n' "$ROOT_DIR/build/lambda.zip" \
  "$(awk "BEGIN { print $(wc -c < "$ROOT_DIR/build/lambda.zip") / 1024 / 1024 }")"
