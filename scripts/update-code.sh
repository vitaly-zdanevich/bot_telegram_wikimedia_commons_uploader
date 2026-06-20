#!/usr/bin/env bash
set -euo pipefail

# Rebuilds and pushes only the Lambda code (no Terraform apply).

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FUNCTION_NAME="${1:-$(terraform -chdir="$ROOT_DIR/infra" output -raw function_name 2>/dev/null || echo telegram-wikimedia-commons-uploader-bot)}"
FUNCTION_URL="$(terraform -chdir="$ROOT_DIR/infra" output -raw function_url 2>/dev/null || true)"
if [[ -n "$FUNCTION_URL" ]]; then
  URL_REGION="$(printf '%s' "$FUNCTION_URL" | sed -n 's#.*lambda-url\.\([a-z0-9-]*\)\.on\.aws.*#\1#p')"
fi
REGION="${URL_REGION:-${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}}"
ZIP_PATH="$ROOT_DIR/build/lambda.zip"

"$ROOT_DIR/scripts/build-lambda.sh"

aws lambda update-function-code \
  --region "$REGION" \
  --function-name "$FUNCTION_NAME" \
  --zip-file "fileb://$ZIP_PATH" \
  >/dev/null

aws lambda wait function-updated \
  --region "$REGION" \
  --function-name "$FUNCTION_NAME"

echo "Updated Lambda code for $FUNCTION_NAME in $REGION"
