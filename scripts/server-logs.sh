#!/usr/bin/env bash
set -euo pipefail
# Reads journald logs for the long-living server deployment (Toolforge / Cloud VPS).
# This is the VM counterpart of show-logs.sh, which reads CloudWatch for the Lambda.

usage() {
  cat <<EOF
Usage: $(basename "$0") [--since 1h] [--follow] [--errors]

Options:
  --since 30s|5m|2h|1d   how far back to read (default: 1h)
  --follow, -f           stream new log lines
  --errors, -e           only lines mentioning error/panic/failed/warn

Environment:
  SERVICE   systemd unit name (default: commons-uploader-bot)

Note: depending on journald permissions this may need to be run with sudo.
EOF
}

SERVICE="${SERVICE:-commons-uploader-bot}"
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

# Converts compact durations (30s, 5m, 2h, 1d) into journalctl --since expressions.
since_expr() {
  case "$SINCE" in
    *s) printf '%s seconds ago' "${SINCE%s}" ;;
    *m) printf '%s minutes ago' "${SINCE%m}" ;;
    *h) printf '%s hours ago' "${SINCE%h}" ;;
    *d) printf '%s days ago' "${SINCE%d}" ;;
    *) printf '%s' "$SINCE" ;;
  esac
}

args=(journalctl -u "$SERVICE" --since "$(since_expr)" --output short-iso)
if [[ "$FOLLOW" == "1" ]]; then
  args+=(-f)
else
  args+=(--no-pager)
fi

if [[ "$ERRORS_ONLY" == "1" ]]; then
  "${args[@]}" | grep -iE 'error|panic|failed|warn' || true
else
  "${args[@]}"
fi
