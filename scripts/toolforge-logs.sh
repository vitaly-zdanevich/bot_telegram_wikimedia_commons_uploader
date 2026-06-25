#!/usr/bin/env bash
set -euo pipefail
# Reads logs from the Toolforge buildservice webservice deployment.

usage() {
  cat <<EOF
Usage: $(basename "$0") [--tail 200] [--since 1h] [--follow] [--errors] [--local]

Options:
  --tail N              read the last N log lines (default: 200)
  --all                 do not limit by tail
  --since 30s|5m|2h     read logs newer than this kubectl duration
  --follow, -f          stream new log lines
  --errors, -e          only lines mentioning error/panic/failed/warn/exception
  --local               run kubectl directly; use after 'become TOOL' on Toolforge

Environment:
  TOOLFORGE_LOGIN       SSH login name (default: vitaly-zdanevich)
  TOOLFORGE_HOST        SSH host (default: login.toolforge.org)
  TOOLFORGE_TOOL        tool account (default: bot-telegram-commons-uploader)
  TOOLFORGE_DEPLOYMENT  Kubernetes deployment (default: TOOLFORGE_TOOL)
  TOOLFORGE_LOG_TARGET  kubectl logs target (default: deploy/TOOLFORGE_DEPLOYMENT)
  TOOLFORGE_SSH         full SSH target override, e.g. user@login.toolforge.org
  TOOLFORGE_SSH_CONFIG  SSH config file override, e.g. /dev/null
  ERROR_PATTERN         grep pattern for --errors
EOF
}

TOOLFORGE_LOGIN="${TOOLFORGE_LOGIN:-vitaly-zdanevich}"
TOOLFORGE_HOST="${TOOLFORGE_HOST:-login.toolforge.org}"
TOOLFORGE_TOOL="${TOOLFORGE_TOOL:-bot-telegram-commons-uploader}"
TOOLFORGE_DEPLOYMENT="${TOOLFORGE_DEPLOYMENT:-$TOOLFORGE_TOOL}"
TOOLFORGE_LOG_TARGET="${TOOLFORGE_LOG_TARGET:-deploy/$TOOLFORGE_DEPLOYMENT}"
TOOLFORGE_SSH="${TOOLFORGE_SSH:-${TOOLFORGE_LOGIN}@${TOOLFORGE_HOST}}"
TOOLFORGE_SSH_CONFIG="${TOOLFORGE_SSH_CONFIG:-}"
ERROR_PATTERN="${ERROR_PATTERN:-error|panic|failed|warn|exception}"

TAIL="${TAIL:-200}"
SINCE="${SINCE:-}"
FOLLOW=0
ERRORS_ONLY=0
LOCAL=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tail) TAIL="$2"; shift 2 ;;
    --all) TAIL=""; shift ;;
    --since) SINCE="$2"; shift 2 ;;
    --follow|-f) FOLLOW=1; shift ;;
    --errors|-e) ERRORS_ONLY=1; shift ;;
    --local) LOCAL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

log_args=(logs "$TOOLFORGE_LOG_TARGET")
if [[ -n "$TAIL" ]]; then
  log_args+=("--tail=$TAIL")
fi
if [[ -n "$SINCE" ]]; then
  log_args+=("--since=$SINCE")
fi
if [[ "$FOLLOW" == "1" ]]; then
  log_args+=(-f)
fi

if [[ "$LOCAL" == "1" ]]; then
  cmd=(kubectl "${log_args[@]}")
else
  ssh_args=()
  if [[ -n "$TOOLFORGE_SSH_CONFIG" ]]; then
    ssh_args=(-F "$TOOLFORGE_SSH_CONFIG")
  else
    # Some sandboxed runners expose /etc/ssh as non-root-owned, which OpenSSH rejects.
    for ssh_config_path in /etc/ssh/ssh_config /etc/ssh/ssh_config.d/*; do
      [[ -e "$ssh_config_path" ]] || continue
      if [[ "$(stat -c %u "$ssh_config_path" 2>/dev/null || printf 0)" != "0" ]]; then
        ssh_args=(-F /dev/null)
        break
      fi
    done
  fi
  cmd=(ssh "${ssh_args[@]}" "$TOOLFORGE_SSH" become "$TOOLFORGE_TOOL" kubectl "${log_args[@]}")
fi

if [[ "$ERRORS_ONLY" == "1" ]]; then
  "${cmd[@]}" | grep --line-buffered -iE "$ERROR_PATTERN" || true
else
  "${cmd[@]}"
fi
