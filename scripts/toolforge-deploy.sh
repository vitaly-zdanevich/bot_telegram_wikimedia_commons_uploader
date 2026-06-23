#!/usr/bin/env bash
set -euo pipefail
# Deploy/refresh the bot on Toolforge as a continuous long-polling job.
#
# Run this as the tool account on a Toolforge bastion (after `become YOURTOOL`).
# One-time: set secrets with `toolforge envvars create …` (see toolforge/README.md).
# Toolforge command names can change between versions — see that README if one fails.

REPO="${REPO:-https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v toolforge >/dev/null 2>&1; then
  echo "The 'toolforge' CLI was not found — run this on a Toolforge bastion as your tool." >&2
  exit 1
fi

echo "==> Building image via the Toolforge Build Service from: $REPO"
toolforge build start "$REPO"
echo "    Track progress with: toolforge build show"

echo "==> Loading continuous jobs from toolforge/jobs.yaml"
echo "    (edit the image: line to tool-<yourtool>/... before first use)"
toolforge jobs load "$ROOT_DIR/toolforge/jobs.yaml"

echo "==> Current jobs:"
toolforge jobs list
echo "Done. Tail logs with: toolforge jobs logs commons-uploader-bot"
