#!/usr/bin/env bash
set -euo pipefail

# Builds the AWS Lambda zip for arm64 (provided.al2023).
#
# By default (HEIC=1) it builds WITH HEIC support, which needs the system libheif
# C/C++ library and therefore a Docker arm64 build (scripts/build-lambda-docker.sh).
# If Docker is unavailable it falls back to a pure cross-build via cargo-lambda/zig
# WITHOUT HEIC (DNG and BMP still convert). Set HEIC=0 to force the fast cross-build.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_NAME="telegram-wikimedia-commons-uploader-bot"
TOOLS_DIR="$ROOT_DIR/.tools"
TARGET="aarch64-unknown-linux-gnu"
RUST_TARGET_CPU="${RUST_TARGET_CPU:-neoverse-n1}"
HEIC="${HEIC:-1}"
OUTPUT_ZIP="$ROOT_DIR/build/lambda.zip"

mkdir -p "$ROOT_DIR/build"

if [[ "$HEIC" == "1" ]]; then
  if command -v docker >/dev/null 2>&1; then
    echo "Building with HEIC support via Docker (arm64)…"
    exec "$ROOT_DIR/scripts/build-lambda-docker.sh"
  fi
  echo "HEIC=1 but Docker is not available; building WITHOUT HEIC (DNG/BMP still convert)." >&2
  echo "Install Docker (with arm64/buildx) or set HEIC=0 to silence this notice." >&2
fi

RUN_USER="${USER:-$(id -un 2>/dev/null || echo local)}"
SAFE_RUN_USER="${RUN_USER//[^a-zA-Z0-9_.-]/_}"
BUILD_CACHE_DIR="$ROOT_DIR/build/cache-$SAFE_RUN_USER"
CARGO_TARGET_DIR="$ROOT_DIR/build/target-$SAFE_RUN_USER"
LAMBDA_DIR="$ROOT_DIR/build/lambda-$SAFE_RUN_USER"

mkdir -p "$BUILD_CACHE_DIR" "$CARGO_TARGET_DIR" "$LAMBDA_DIR"
export XDG_CACHE_HOME="$BUILD_CACHE_DIR"
export CARGO_TARGET_DIR

install_local_rustup() {
  local arch rustup_arch tmp_dir
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) rustup_arch="x86_64" ;;
    aarch64 | arm64) rustup_arch="aarch64" ;;
    *) echo "Unsupported host architecture: $arch" >&2; exit 1 ;;
  esac

  export RUSTUP_HOME="$TOOLS_DIR/rustup"
  export CARGO_HOME="$TOOLS_DIR/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"

  if [[ -x "$CARGO_HOME/bin/rustup" ]]; then
    return
  fi

  echo "rustup not found; installing a project-local Rust toolchain into $TOOLS_DIR"
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' RETURN
  curl -fsSL -o "$tmp_dir/rustup-init" \
    "https://static.rust-lang.org/rustup/dist/${rustup_arch}-unknown-linux-gnu/rustup-init"
  chmod +x "$tmp_dir/rustup-init"
  "$tmp_dir/rustup-init" -y --no-modify-path --profile minimal \
    --default-toolchain stable --target "$TARGET"
}

if command -v rustup >/dev/null 2>&1; then
  rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
else
  install_local_rustup
  rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
fi

export PATH="$TOOLS_DIR/bin:$PATH"
if [[ -x "$TOOLS_DIR/bin/cargo-lambda" ]]; then
  CARGO_LAMBDA="$TOOLS_DIR/bin/cargo-lambda"
elif command -v cargo-lambda >/dev/null 2>&1; then
  CARGO_LAMBDA="$(command -v cargo-lambda)"
else
  echo "cargo-lambda not found; installing it into $TOOLS_DIR"
  cargo install cargo-lambda --root "$TOOLS_DIR"
  CARGO_LAMBDA="$TOOLS_DIR/bin/cargo-lambda"
fi

CURRENT_RUSTFLAGS="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS:-}"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${CURRENT_RUSTFLAGS:+$CURRENT_RUSTFLAGS }-C target-cpu=${RUST_TARGET_CPU}"

rm -rf "$LAMBDA_DIR/$BIN_NAME"
echo "Building $BIN_NAME for $TARGET with target-cpu=$RUST_TARGET_CPU (no HEIC)"
"$CARGO_LAMBDA" lambda build \
  --manifest-path "$ROOT_DIR/Cargo.toml" \
  --release --arm64 \
  --no-default-features \
  --lambda-dir "$LAMBDA_DIR" \
  --output-format zip \
  --bin "$BIN_NAME"

zip_candidate="$LAMBDA_DIR/$BIN_NAME/bootstrap.zip"
if [[ ! -f "$zip_candidate" ]]; then
  zip_candidate="$(find "$LAMBDA_DIR" -maxdepth 4 -type f -name '*.zip' | sort | head -n 1)"
fi
if [[ -z "$zip_candidate" || ! -f "$zip_candidate" ]]; then
  echo "cargo-lambda did not produce a Lambda zip under $LAMBDA_DIR" >&2
  exit 1
fi

cp "$zip_candidate" "$OUTPUT_ZIP"
printf 'Wrote %s (%.1f MB)\n' "$OUTPUT_ZIP" "$(awk "BEGIN { print $(wc -c < "$OUTPUT_ZIP") / 1024 / 1024 }")"
