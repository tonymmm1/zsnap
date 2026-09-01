#!/bin/sh
set -eu

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  if ! systemctl enable --now zsnap.timer; then
    echo "warning: zsnap.timer could not be enabled; run 'systemctl enable --now zsnap.timer' after installation" >&2
  fi
elif command -v rc-update >/dev/null 2>&1; then
  rc-update add crond default >/dev/null 2>&1 || :
  rc-service crond start >/dev/null 2>&1 || :
fi

exit 0
