#!/bin/sh
set -eu

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
  # On a true removal the packaged unit is gone. During an upgrade it remains,
  # and the replacement package's post-install hook keeps the timer active.
  if [ ! -e /usr/lib/systemd/system/zsnap.timer ]; then
    systemctl disable --now zsnap.timer >/dev/null 2>&1 || :
  fi
  systemctl daemon-reload >/dev/null 2>&1 || :
fi

exit 0
