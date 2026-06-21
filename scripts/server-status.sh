#!/usr/bin/env bash
set -uo pipefail
# Quick health check for the long-living server deployment (Toolforge / Cloud VPS):
# service state, memory headroom, recent errors, and (optionally) a Telegram getMe ping.

SERVICE="${SERVICE:-commons-uploader-bot}"
BIN="${BIN:-telegram-wikimedia-commons-uploader-bot}"

echo "== Service: $SERVICE =="
printf 'active:  '; systemctl is-active "$SERVICE" 2>/dev/null || true
printf 'enabled: '; systemctl is-enabled "$SERVICE" 2>/dev/null || true
echo
systemctl status "$SERVICE" --no-pager -n 5 2>/dev/null || echo "(systemctl status unavailable)"

echo
echo "== Memory =="
if [[ -r /proc/meminfo ]]; then
  grep -E '^(MemTotal|MemAvailable):' /proc/meminfo
fi
ps -o pid,rss,etimes,cmd -C "$BIN" 2>/dev/null || echo "(process '$BIN' not found)"

echo
echo "== Recent errors (last 1h) =="
journalctl -u "$SERVICE" --since "1 hour ago" --no-pager 2>/dev/null \
  | grep -iE 'error|panic|failed' | tail -n 10 || echo "(none, or journald unavailable)"

if [[ -n "${TELEGRAM_BOT_TOKEN:-}" ]]; then
  base="${TELEGRAM_API_BASE:-https://api.telegram.org}"
  echo
  echo "== Telegram getMe ($base) =="
  if command -v jq >/dev/null 2>&1; then
    curl -fsS "$base/bot${TELEGRAM_BOT_TOKEN}/getMe" 2>/dev/null \
      | jq -r '.result | "@\(.username) (id \(.id))"' || echo "(getMe failed)"
  else
    curl -fsS "$base/bot${TELEGRAM_BOT_TOKEN}/getMe" 2>/dev/null || echo "(getMe failed)"
  fi
fi
