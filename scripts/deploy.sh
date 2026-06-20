#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

"$ROOT_DIR/scripts/build-lambda.sh"
terraform -chdir="$ROOT_DIR/infra" init
terraform -chdir="$ROOT_DIR/infra" apply
"$ROOT_DIR/scripts/set-webhook.sh"
