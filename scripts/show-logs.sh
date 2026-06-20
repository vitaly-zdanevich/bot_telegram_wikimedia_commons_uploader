#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<EOF
Usage: $(basename "$0") [--since 1h] [--follow] [--errors]

Environment:
  AWS_REGION or AWS_DEFAULT_REGION
  FUNCTION_NAME
EOF
}

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FUNCTION_NAME="${FUNCTION_NAME:-$(terraform -chdir="$ROOT_DIR/infra" output -raw function_name 2>/dev/null || echo telegram-wikimedia-commons-uploader-bot)}"
FUNCTION_URL="$(terraform -chdir="$ROOT_DIR/infra" output -raw function_url 2>/dev/null || true)"
if [[ -n "$FUNCTION_URL" ]]; then
  URL_REGION="$(printf '%s' "$FUNCTION_URL" | sed -n 's#.*lambda-url\.\([a-z0-9-]*\)\.on\.aws.*#\1#p')"
fi
AWS_REGION="${URL_REGION:-${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}}"
SINCE="${SINCE:-1h}"
FOLLOW=0
ERRORS_ONLY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --since) SINCE="$2"; shift 2 ;;
    --follow|-f) FOLLOW=1; shift ;;
    --errors|-e) ERRORS_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

LOG_GROUP="/aws/lambda/${FUNCTION_NAME}"

# Converts compact durations (30s, 5m, 2h, 1d) into GNU date expressions.
since_expr() {
  case "$SINCE" in
    *s) printf '%s seconds ago' "${SINCE%s}" ;;
    *m) printf '%s minutes ago' "${SINCE%m}" ;;
    *h) printf '%s hours ago' "${SINCE%h}" ;;
    *d) printf '%s days ago' "${SINCE%d}" ;;
    *) printf '%s ago' "$SINCE" ;;
  esac
}

START_MS="$(( $(date -u -d "$(since_expr)" +%s) * 1000 ))"
FILTER_PATTERN=""
if [[ "$ERRORS_ONLY" == "1" ]]; then
  FILTER_PATTERN='?ERROR ?Error ?error ?panic ?PANIC ?failed'
fi

fetch_logs() {
  local args=(logs filter-log-events
    --region "$AWS_REGION"
    --log-group-name "$LOG_GROUP"
    --start-time "$START_MS"
    --interleaved
    --output json)
  if [[ -n "$FILTER_PATTERN" ]]; then
    args+=(--filter-pattern "$FILTER_PATTERN")
  fi
  aws "${args[@]}" | jq -r '.events[] | "\(.timestamp) \(.logStreamName) \(.message)"'
}

if [[ "$FOLLOW" != "1" ]]; then
  fetch_logs
  exit 0
fi

while true; do
  fetch_logs
  START_MS="$(( $(date -u +%s) * 1000 ))"
  sleep 10
done
