#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 [--static] [--no-enable] [--uninstall]"
}

enable_timer=1
uninstall=0
static_binary=0
while (($#)); do
  case "$1" in
    --static)
      static_binary=1
      ;;
    --no-enable)
      enable_timer=0
      ;;
    --uninstall)
      uninstall=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
  shift
done

run_privileged() {
  if ((EUID == 0)); then
    "$@"
  else
    sudo "$@"
  fi
}

if ((uninstall)); then
  run_privileged make uninstall
  exit 0
fi

if ((static_binary)); then
  make static
  run_privileged make install-static
else
  make release
  run_privileged make install
fi
if ((enable_timer)); then
  if run_privileged /usr/local/sbin/zsnap --config /etc/zsnap/zsnap.toml check --probe; then
    run_privileged make enable
  else
    echo >&2
    echo "The binary and units were installed, but the timer was not enabled because" >&2
    echo "/etc/zsnap/zsnap.toml does not yet resolve on this host." >&2
    echo "Edit it, run 'sudo zsnap check --probe', then run 'sudo make enable'." >&2
    enable_timer=0
  fi
fi

echo
echo "zsnap is installed. Review /etc/zsnap/zsnap.toml before the first mutating run."
echo "Preview changes with: sudo zsnap plan"
if ((enable_timer == 0)); then
  echo "The systemd unit is installed but its timer is not active."
fi
