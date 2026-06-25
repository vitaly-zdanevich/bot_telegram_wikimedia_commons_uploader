#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TFVARS_FILE="$ROOT_DIR/infra/terraform.tfvars"

# Reads a simple quoted string variable from the local, gitignored tfvars file.
tfvars_string() {
  local key="$1"
  if [[ -f "$TFVARS_FILE" ]]; then
    sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"\\(.*\\)\"[[:space:]]*$/\\1/p" "$TFVARS_FILE" | tail -n 1
  fi
}

TOKEN="${TELEGRAM_BOT_TOKEN:-${TF_VAR_telegram_bot_token:-$(tfvars_string telegram_bot_token)}}"
SECRET="${TELEGRAM_WEBHOOK_SECRET:-${TF_VAR_telegram_webhook_secret:-$(tfvars_string telegram_webhook_secret)}}"
WEBHOOK_URL="${WEBHOOK_URL:-${FUNCTION_URL:-}}"
if [[ -z "$WEBHOOK_URL" && -n "${TOOLFORGE_TOOL:-}" ]]; then
  WEBHOOK_URL="https://${TOOLFORGE_TOOL}.toolforge.org/telegram"
fi
if [[ -z "$WEBHOOK_URL" ]]; then
  WEBHOOK_URL="$(terraform -chdir="$ROOT_DIR/infra" output -raw function_url)"
fi

if [[ -z "$TOKEN" ]]; then
  echo "TELEGRAM_BOT_TOKEN or TF_VAR_telegram_bot_token is required" >&2
  exit 1
fi
if [[ -z "$SECRET" ]]; then
  echo "TELEGRAM_WEBHOOK_SECRET or TF_VAR_telegram_webhook_secret is required" >&2
  exit 1
fi

curl -fsS \
  -X POST \
  "https://api.telegram.org/bot${TOKEN}/setWebhook" \
  -H 'content-type: application/json' \
  -d "{\"url\":\"${WEBHOOK_URL}\",\"secret_token\":\"${SECRET}\",\"allowed_updates\":[\"message\",\"callback_query\"]}"

echo
echo "Webhook set to $WEBHOOK_URL"
