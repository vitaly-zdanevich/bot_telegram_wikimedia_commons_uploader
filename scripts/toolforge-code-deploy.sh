#!/usr/bin/env bash
set -euo pipefail

# Toolforge code-only deploy.
#
# This builds a portable Rust executable, uploads only that executable into
# project storage, and restarts the existing buildservice image. It intentionally
# does not rebuild Aptfile packages and does not read, upload, delete, or rewrite
# /data/project/<tool>/ytdlp-cookies.txt; update cookies separately with
# scripts/toolforge-upload-ytdlp-cookies.sh.
#
# Use scripts/toolforge-webhook-deploy.sh instead when Aptfile, Procfile,
# scripts/run-toolforge-webhook.sh, project.toml, or toolforge/service.template
# changes.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TOOLFORGE_USER="${TOOLFORGE_USER:-vitaly-zdanevich}"
TOOLFORGE_HOST="${TOOLFORGE_HOST:-login.toolforge.org}"
TOOLFORGE_TOOL="${TOOLFORGE_TOOL:-bot-telegram-commons-uploader}"
BIN_NAME="${BIN_NAME:-telegram-wikimedia-commons-uploader-bot}"
FEATURES="${FEATURES:-sqlite,archive,rar,heic}"
BUILD_BACKEND="${BUILD_BACKEND:-docker}"
DEFAULT_DOCKER_IMAGE="telegram-commons-toolforge-rust:ubuntu24.04"
DOCKER_IMAGE="${DOCKER_IMAGE:-$DEFAULT_DOCKER_IMAGE}"
DOCKER_BASE_IMAGE="${DOCKER_BASE_IMAGE:-ubuntu:24.04}"
DOCKER_TARGET_DIR="${DOCKER_TARGET_DIR:-target/toolforge-docker}"
LOCAL_BIN="${LOCAL_BIN:-}"
REMOTE_DIR="${REMOTE_DIR:-/data/project/$TOOLFORGE_TOOL/bin}"
REMOTE_BIN="${REMOTE_BIN:-$REMOTE_DIR/$BIN_NAME}"
SSH_TARGET="$TOOLFORGE_USER@$TOOLFORGE_HOST"
REMOTE_TMP="/tmp/$BIN_NAME.$(date +%s).$$"
HEALTH_URL="${HEALTH_URL:-https://$TOOLFORGE_TOOL.toolforge.org/healthz}"

ensure_default_docker_image() {
  if [[ "$DOCKER_IMAGE" != "$DEFAULT_DOCKER_IMAGE" ]]; then
    return
  fi
  if [[ "${REBUILD_DOCKER_IMAGE:-0}" != 1 ]] \
    && docker image inspect "$DOCKER_IMAGE" >/dev/null 2>&1 \
    && docker run --rm "$DOCKER_IMAGE" pkg-config --exists "libheif >= 1.17"; then
    return
  fi

  echo "==> Building cached Toolforge-compatible Rust Docker image: $DOCKER_IMAGE"
  docker build \
    --build-arg "DOCKER_BASE_IMAGE=$DOCKER_BASE_IMAGE" \
    --tag "$DOCKER_IMAGE" \
    - <<'DOCKERFILE'
ARG DOCKER_BASE_IMAGE=ubuntu:24.04
FROM ${DOCKER_BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH=/usr/local/cargo/bin:${PATH}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      curl \
      libde265-dev \
      libheif-dev \
      pkg-config \
      zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
    && chmod -R a+w /usr/local/cargo /usr/local/rustup
DOCKERFILE
}

if [[ "$TOOLFORGE_USER" == *"'"* || "$TOOLFORGE_HOST" == *"'"* || "$TOOLFORGE_TOOL" == *"'"* ]]; then
  echo "Toolforge user, host, and tool names must not contain single quotes." >&2
  exit 1
fi

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  case "$BUILD_BACKEND" in
    docker)
      ensure_default_docker_image
      echo "==> Building portable release binary in Docker"
      docker run --rm \
        --user "$(id -u):$(id -g)" \
        --volume "$ROOT_DIR:/work" \
        --workdir /work \
        --env CARGO_HOME=/work/target/toolforge-cargo-home \
        --env CARGO_TARGET_DIR="/work/$DOCKER_TARGET_DIR" \
        --env RUSTFLAGS="${RUSTFLAGS:--C target-cpu=x86-64}" \
        --env CFLAGS="${CFLAGS:--march=x86-64 -mtune=generic}" \
        --env CXXFLAGS="${CXXFLAGS:--march=x86-64 -mtune=generic}" \
        "$DOCKER_IMAGE" \
        cargo build --locked --release --features "$FEATURES" --bin "$BIN_NAME"
      LOCAL_BIN="${LOCAL_BIN:-$ROOT_DIR/$DOCKER_TARGET_DIR/release/$BIN_NAME}"
      ;;
    host)
      echo "==> Building local release binary on host"
      export RUSTFLAGS="${RUSTFLAGS:--C target-cpu=x86-64}"
      export CFLAGS="${CFLAGS:--march=x86-64 -mtune=generic}"
      export CXXFLAGS="${CXXFLAGS:--march=x86-64 -mtune=generic}"
      cargo build --locked --release --features "$FEATURES" --bin "$BIN_NAME"
      LOCAL_BIN="${LOCAL_BIN:-$ROOT_DIR/target/release/$BIN_NAME}"
      ;;
    *)
      echo "Unknown BUILD_BACKEND=$BUILD_BACKEND; expected docker or host." >&2
      exit 1
      ;;
  esac
else
  LOCAL_BIN="${LOCAL_BIN:-$ROOT_DIR/$DOCKER_TARGET_DIR/release/$BIN_NAME}"
  echo "==> SKIP_BUILD=1, using existing binary: $LOCAL_BIN"
fi

if [[ ! -x "$LOCAL_BIN" ]]; then
  echo "Executable not found: $LOCAL_BIN" >&2
  exit 1
fi

if readelf -n "$LOCAL_BIN" | grep -E 'x86 ISA needed:.*x86-64-v[234]' >/dev/null; then
  echo "Refusing to upload $LOCAL_BIN because it requires a newer x86-64 ISA than Toolforge guarantees." >&2
  readelf -n "$LOCAL_BIN" | grep -E 'x86 ISA needed:' >&2 || true
  echo "Use BUILD_BACKEND=docker with a portable Rust image, or build on Toolforge-compatible hardware." >&2
  exit 1
fi

echo "==> Creating remote binary directory"
ssh "$SSH_TARGET" "become '$TOOLFORGE_TOOL' install -d -m 700 '$REMOTE_DIR'"

echo "==> Uploading $LOCAL_BIN to $SSH_TARGET:$REMOTE_BIN"
scp "$LOCAL_BIN" "$SSH_TARGET:$REMOTE_TMP"
ssh "$SSH_TARGET" "become '$TOOLFORGE_TOOL' install -m 755 '$REMOTE_TMP' '$REMOTE_BIN'"
ssh "$SSH_TARGET" "rm -f '$REMOTE_TMP'"

echo "==> Pointing the webservice at the uploaded binary"
ssh "$SSH_TARGET" "become '$TOOLFORGE_TOOL' toolforge envvars create BOT_BIN '$REMOTE_BIN'"

echo "==> Restarting Toolforge webservice"
if ! ssh "$SSH_TARGET" "become '$TOOLFORGE_TOOL' toolforge webservice --template service.template restart"; then
  echo "Toolforge restart command did not report success; checking $HEALTH_URL"
  restart_ok=0
  for _ in $(seq 1 30); do
    if curl -fsS "$HEALTH_URL" >/dev/null 2>&1; then
      restart_ok=1
      break
    fi
    sleep 2
  done
  if [[ "$restart_ok" != 1 ]]; then
    echo "Webservice did not become healthy at $HEALTH_URL after restart timeout." >&2
    exit 1
  fi
  echo "Webservice is healthy."
fi

echo "Done. Aptfile packages were not rebuilt. yt-dlp cookies were left unchanged."
