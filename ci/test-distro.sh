#!/bin/sh
set -eu

. /etc/os-release

case "${ID:-}" in
  ubuntu|debian)
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends build-essential ca-certificates curl
    ;;
  fedora|rocky|rhel|centos)
    set -- gcc make ca-certificates tar gzip
    if ! command -v curl >/dev/null 2>&1; then
      set -- "$@" curl-minimal
    fi
    if command -v dnf >/dev/null 2>&1; then
      dnf install -y "$@"
    else
      microdnf install -y "$@"
    fi
    ;;
  alpine)
    apk add --no-cache build-base ca-certificates curl
    ;;
  arch)
    pacman -Syu --needed --noconfirm base-devel ca-certificates curl
    ;;
  *)
    echo "unsupported CI image: ${ID:-unknown}" >&2
    exit 2
    ;;
esac

ci_work=/tmp/zsnap-distro-ci
mkdir -p "$ci_work"
cp -R /source/. "$ci_work/"

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
  sh -s -- -y --profile minimal --default-toolchain 1.85.0
export PATH="/root/.cargo/bin:$PATH"

cd "$ci_work"
cargo test --all-targets --locked
cargo build --release --locked
./target/release/zsnap --config ./config.example.toml check
