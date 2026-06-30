#!/usr/bin/env bash
set -euo pipefail

# Upload a yt-dlp Netscape cookies file to Toolforge persistent storage.
# Normal code deploys do not touch this file.

usage() {
  cat <<EOF
Usage: $(basename "$0") [path/to/cookies.txt]

Defaults:
  local cookies:       /tmp/cookies-youtube-com.txt
  remote cookie path:  /data/project/\$TOOLFORGE_TOOL/ytdlp-cookies.txt

Environment:
  TOOLFORGE_LOGIN       SSH login name (default: vitaly-zdanevich)
  TOOLFORGE_HOST        SSH host (default: login.toolforge.org)
  TOOLFORGE_TOOL        tool account (default: bot-telegram-commons-uploader)
  TOOLFORGE_SSH         full SSH target override, e.g. user@login.toolforge.org
  TOOLFORGE_SSH_CONFIG  SSH config file override, e.g. /dev/null
  YTDLP_COOKIES_PATH    remote cookie path override
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

LOCAL_COOKIES="${1:-/tmp/cookies-youtube-com.txt}"
TOOLFORGE_LOGIN="${TOOLFORGE_LOGIN:-vitaly-zdanevich}"
TOOLFORGE_HOST="${TOOLFORGE_HOST:-login.toolforge.org}"
TOOLFORGE_TOOL="${TOOLFORGE_TOOL:-bot-telegram-commons-uploader}"
TOOLFORGE_SSH="${TOOLFORGE_SSH:-${TOOLFORGE_LOGIN}@${TOOLFORGE_HOST}}"
REMOTE_COOKIES="${YTDLP_COOKIES_PATH:-/data/project/${TOOLFORGE_TOOL}/ytdlp-cookies.txt}"
TOOLFORGE_SSH_CONFIG="${TOOLFORGE_SSH_CONFIG:-}"

if [[ ! -r "$LOCAL_COOKIES" ]]; then
  echo "Cookies file is not readable: $LOCAL_COOKIES" >&2
  echo "Export YouTube cookies in Netscape format, then rerun this script." >&2
  exit 1
fi

ssh_args=()
if [[ -n "$TOOLFORGE_SSH_CONFIG" ]]; then
  ssh_args=(-F "$TOOLFORGE_SSH_CONFIG")
fi

ssh "${ssh_args[@]}" "$TOOLFORGE_SSH" become "$TOOLFORGE_TOOL" sh -c '
  set -eu
  dest="$1"
  tmp="${dest}.tmp.$$"
  umask 077
  cat > "$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$dest"
  ls -l "$dest"
' sh "$REMOTE_COOKIES" <"$LOCAL_COOKIES"

echo "Uploaded yt-dlp cookies to $REMOTE_COOKIES"
