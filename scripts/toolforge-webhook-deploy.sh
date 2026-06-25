#!/usr/bin/env bash
set -euo pipefail

# Deploy/refresh the bot on Toolforge as a public webhook webservice.
#
# Run this as the tool account on a Toolforge bastion (after `become YOURTOOL`).
# One-time: set secrets with `toolforge envvars create …` (see toolforge/README.md).

REPO="${REPO:-https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v toolforge >/dev/null 2>&1; then
  echo "The 'toolforge' CLI was not found — run this on a Toolforge bastion as your tool." >&2
  exit 1
fi

if [[ "${LOGNAME:-}" != tools.* ]]; then
  echo "Run this after 'become <toolname>' so Toolforge can load the tool kubeconfig." >&2
  exit 1
fi

echo "==> Installing webservice template in \$HOME/service.template"
cp "$ROOT_DIR/toolforge/service.template" "$HOME/service.template"

echo "==> Building image via the Toolforge Build Service from: $REPO"
toolforge build start "$REPO"
echo "    Track progress with: toolforge build show"
echo "    After a successful build, restart with: toolforge webservice restart"

echo "==> Starting Toolforge buildservice webservice"
toolforge webservice buildservice start --mount=all --health-check-path=/healthz

echo "==> Webservice status:"
toolforge webservice status
