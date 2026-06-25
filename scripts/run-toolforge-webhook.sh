#!/usr/bin/env bash
set -euo pipefail

BOT_BIN="${BOT_BIN:-./target/release/telegram-wikimedia-commons-uploader-bot}"
PORT="${PORT:-8000}"
BOT_API_PORT="${TELEGRAM_BOT_API_PORT:-8081}"
TOOL_DATA_DIR="${TOOL_DATA_DIR:-/data/project/${TOOLFORGE_TOOL:-bot-telegram-commons-uploader}}"
BOT_API_BIN="${TELEGRAM_BOT_API_BIN:-$TOOL_DATA_DIR/bin/telegram-bot-api}"
BOT_API_DIR="${TELEGRAM_BOT_API_DIR:-$TOOL_DATA_DIR/telegram-bot-api}"
BOT_API_LOG="${TELEGRAM_BOT_API_LOG:-$TOOL_DATA_DIR/telegram-bot-api.log}"

pids=()
cleanup() {
  for pid in "${pids[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

wait_for_url() {
  local url="$1"
  local attempts="${2:-60}"
  for _ in $(seq 1 "$attempts"); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "Timed out waiting for $url" >&2
  return 1
}

use_local_api=0
if [[ -x "$BOT_API_BIN" ]]; then
  if [[ -n "${TELEGRAM_API_ID:-}" && -n "${TELEGRAM_API_HASH:-}" ]]; then
    mkdir -p "$BOT_API_DIR"
    "$BOT_API_BIN" \
      --local \
      --http-ip-address=127.0.0.1 \
      --http-port="$BOT_API_PORT" \
      --dir="$BOT_API_DIR" \
      ${TELEGRAM_BOT_API_EXTRA_ARGS:-} >"$BOT_API_LOG" 2>&1 &
    pids+=("$!")
    export TELEGRAM_API_BASE="http://127.0.0.1:$BOT_API_PORT"
    use_local_api=1
  else
    echo "telegram-bot-api binary exists, but TELEGRAM_API_ID/HASH are not set; using cloud Bot API." >&2
  fi
else
  echo "telegram-bot-api binary not found at $BOT_API_BIN; using cloud Bot API." >&2
fi

"$BOT_BIN" &
bot_pid="$!"
pids+=("$bot_pid")

if [[ "$use_local_api" == 1 ]]; then
  wait_for_url "http://127.0.0.1:$PORT/healthz"

  if [[ "${TELEGRAM_BOT_API_CLOUD_LOGOUT:-0}" == 1 ]]; then
    curl -fsS "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/logOut" >/dev/null || true
  fi

  wait_for_url "http://127.0.0.1:$BOT_API_PORT/bot${TELEGRAM_BOT_TOKEN}/getMe"
  curl -fsS \
    -X POST \
    "http://127.0.0.1:${BOT_API_PORT}/bot${TELEGRAM_BOT_TOKEN}/setWebhook" \
    --data-urlencode "url=http://127.0.0.1:${PORT}/telegram" \
    --data-urlencode "secret_token=${TELEGRAM_WEBHOOK_SECRET}" \
    --data-urlencode 'allowed_updates=["message","callback_query"]' \
    >/dev/null
fi

wait "$bot_pid"
