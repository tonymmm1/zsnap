#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: ./install.sh [options]

  --static              Build a portable musl-linked binary
  --static-target TRIPLE
                        Override the automatically selected musl target
  --init SYSTEM         auto, systemd, openrc, or none (default: auto)
  --no-enable           Install without enabling a schedule
  --install-deps        Install compiler/build prerequisites with the host package manager
  --bootstrap-rust      Install the pinned Rust toolchain with official rustup when needed
  --uninstall           Remove installed program and scheduler files; preserve configuration
  -h, --help            Show this help
EOF
}

enable_schedule=1
install_dependencies=0
bootstrap_rust=0
uninstall=0
static_binary=0
static_target=""
init_system=auto

while [ "$#" -gt 0 ]; do
  case "$1" in
    --static)
      static_binary=1
      ;;
    --static-target)
      if [ "$#" -lt 2 ]; then
        echo "--static-target requires a value" >&2
        exit 2
      fi
      static_binary=1
      static_target=$2
      shift
      ;;
    --init)
      if [ "$#" -lt 2 ]; then
        echo "--init requires a value" >&2
        exit 2
      fi
      init_system=$2
      shift
      ;;
    --no-enable)
      enable_schedule=0
      ;;
    --install-deps)
      install_dependencies=1
      ;;
    --bootstrap-rust)
      bootstrap_rust=1
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

case "$init_system" in
  auto|systemd|openrc|none) ;;
  *) echo "unsupported init system: $init_system" >&2; exit 2 ;;
esac

project_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
cd "$project_dir"

run_privileged() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    echo "root privileges are required, but sudo is not installed" >&2
    return 1
  fi
}

detect_init_system() {
  if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    echo systemd
  elif command -v rc-service >/dev/null 2>&1; then
    echo openrc
  else
    echo none
  fi
}

if [ "$init_system" = auto ]; then
  init_system=$(detect_init_system)
fi

if [ "$uninstall" -eq 1 ]; then
  case "$init_system" in
    systemd) run_privileged make uninstall-systemd ;;
    openrc) run_privileged make uninstall-openrc ;;
    none) run_privileged make uninstall-common ;;
  esac
  exit 0
fi

install_build_dependencies() {
  if command -v apt-get >/dev/null 2>&1; then
    run_privileged apt-get update
    run_privileged apt-get install -y build-essential curl ca-certificates
  elif command -v dnf >/dev/null 2>&1; then
    run_privileged dnf install -y gcc make curl ca-certificates
  elif command -v yum >/dev/null 2>&1; then
    run_privileged yum install -y gcc make curl ca-certificates
  elif command -v apk >/dev/null 2>&1; then
    run_privileged apk add --no-cache build-base curl ca-certificates
  elif command -v pacman >/dev/null 2>&1; then
    run_privileged pacman -Syu --needed --noconfirm base-devel curl ca-certificates
  else
    echo "unsupported package manager; install a C compiler, make, curl, and CA certificates" >&2
    return 1
  fi
}

if [ "$install_dependencies" -eq 1 ]; then
  install_build_dependencies
fi

if [ "$bootstrap_rust" -eq 1 ] && ! command -v rustup >/dev/null 2>&1; then
  if ! command -v curl >/dev/null 2>&1; then
    echo "curl is required to bootstrap Rust; use --install-deps first" >&2
    exit 1
  fi
  rustup_installer=$(mktemp "${TMPDIR:-/tmp}/zsnap-rustup.XXXXXX")
  trap 'rm -f "$rustup_installer"' 0 HUP INT TERM
  curl --proto '=https' --tlsv1.2 -sSf -o "$rustup_installer" https://sh.rustup.rs
  sh "$rustup_installer" -y --profile minimal --default-toolchain 1.85.0
  rm -f "$rustup_installer"
  trap - 0 HUP INT TERM
  rustup_environment="${CARGO_HOME:-${HOME:-/root}/.cargo}/env"
  # shellcheck disable=SC1090
  . "$rustup_environment"
fi

for command_name in cargo make install; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "missing build command '$command_name'" >&2
    echo "rerun with --install-deps --bootstrap-rust, or install the prerequisites manually" >&2
    exit 1
  fi
done

if [ "$static_binary" -eq 1 ]; then
  if ! command -v cc >/dev/null 2>&1; then
    echo "--static requires a C compiler for the bundled TLS crypto; use --install-deps" >&2
    exit 1
  fi
  if [ -z "$static_target" ]; then
    case "$(uname -m)" in
      x86_64|amd64) static_target=x86_64-unknown-linux-musl ;;
      aarch64|arm64) static_target=aarch64-unknown-linux-musl ;;
      *)
        echo "cannot select a musl target for $(uname -m); pass --static-target" >&2
        exit 1
        ;;
    esac
  fi
  if ! command -v rustup >/dev/null 2>&1; then
    echo "--static requires rustup so the $static_target standard library can be installed" >&2
    exit 1
  fi
  rustup target add "$static_target"
  musl_compiler=cc
  make static STATIC_TARGET="$static_target" MUSL_CC="$musl_compiler"
  case "$init_system" in
    systemd) install_target=install-static ;;
    openrc) install_target=install-static-openrc ;;
    none) install_target=install-static-none ;;
  esac
  run_privileged make "$install_target" STATIC_TARGET="$static_target" MUSL_CC="$musl_compiler"
else
  make release
  case "$init_system" in
    systemd) install_target=install ;;
    openrc) install_target=install-openrc ;;
    none) install_target=install-none ;;
  esac
  run_privileged make "$install_target"
fi

if [ "$enable_schedule" -eq 1 ] && [ "$init_system" != none ]; then
  if run_privileged /usr/local/sbin/zsnap --config /etc/zsnap/zsnap.toml check --probe; then
    case "$init_system" in
      systemd) run_privileged make enable ;;
      openrc) run_privileged make enable-openrc ;;
    esac
  else
    echo >&2
    echo "The binary and scheduler files were installed, but scheduling was not enabled because" >&2
    echo "/etc/zsnap/zsnap.toml does not yet resolve on this host." >&2
    echo "Edit it, run 'sudo zsnap check --probe', then enable the matching scheduler." >&2
    enable_schedule=0
  fi
fi

echo
echo "zsnap is installed for $init_system. Review /etc/zsnap/zsnap.toml before the first mutating run."
echo "Preview changes with: sudo zsnap plan"
if [ "$enable_schedule" -eq 0 ] || [ "$init_system" = none ]; then
  echo "No recurring schedule was enabled."
fi
