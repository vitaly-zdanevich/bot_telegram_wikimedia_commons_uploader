#!/usr/bin/env bash
set -euo pipefail

# Toolforge code-only deploy. This intentionally does not read, upload, delete, or rewrite
# /data/project/<tool>/ytdlp-cookies.txt; update cookies separately with
# scripts/toolforge-upload-ytdlp-cookies.sh.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Deploying Toolforge code only. yt-dlp cookies are left unchanged."
exec "$ROOT_DIR/scripts/toolforge-webhook-deploy.sh"
