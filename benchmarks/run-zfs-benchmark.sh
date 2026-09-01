#!/usr/bin/env bash
# Resolve temporary upstream Sanoid baselines, then run the ZFS harness.

set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
SANOID_REPOSITORY=${SANOID_REPOSITORY:-https://github.com/jimsalterjrs/sanoid.git}
SANOID_STABLE_REF=${SANOID_STABLE_REF:-v2.3.0}
SANOID_DEVELOPMENT_REF=${SANOID_DEVELOPMENT_REF:-master}
SANOID_STABLE_BIN=${SANOID_STABLE_BIN:-${SANOID_BIN:-}}
SANOID_STABLE_DEFAULTS=${SANOID_STABLE_DEFAULTS:-${SANOID_DEFAULTS:-}}
SANOID_DEVELOPMENT_BIN=${SANOID_DEVELOPMENT_BIN:-}
SANOID_DEVELOPMENT_DEFAULTS=${SANOID_DEVELOPMENT_DEFAULTS:-}
SANOID_DEVELOPMENT_REVISION=${SANOID_DEVELOPMENT_REVISION:-}
DEPENDENCY_DIR=

die() { printf 'benchmark setup error: %s\n' "$*" >&2; exit 1; }

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM
    if [[ -n $DEPENDENCY_DIR ]]; then
        if [[ $DEPENDENCY_DIR =~ ^/var/tmp/zsnap-sanoid-deps\.[A-Za-z0-9]+$ && -d $DEPENDENCY_DIR ]]; then
            rm -rf -- "$DEPENDENCY_DIR"
        else
            printf 'SAFETY: refusing to remove unexpected dependency path %q\n' "$DEPENDENCY_DIR" >&2
        fi
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

if [[ -z $SANOID_STABLE_BIN || -z $SANOID_DEVELOPMENT_BIN ]]; then
    command -v git >/dev/null || die "git is required to fetch temporary Sanoid baselines"
    DEPENDENCY_DIR=$(mktemp -d /var/tmp/zsnap-sanoid-deps.XXXXXX)
fi

if [[ -z $SANOID_STABLE_BIN ]]; then
    printf 'Fetching temporary Sanoid stable ref %s...\n' "$SANOID_STABLE_REF"
    git -c advice.detachedHead=false clone --quiet --depth 1 --single-branch --branch "$SANOID_STABLE_REF" \
        "$SANOID_REPOSITORY" "$DEPENDENCY_DIR/stable" || die "could not fetch Sanoid stable"
    SANOID_STABLE_BIN=$DEPENDENCY_DIR/stable/sanoid
    SANOID_STABLE_DEFAULTS=$DEPENDENCY_DIR/stable/sanoid.defaults.conf
fi

if [[ -z $SANOID_DEVELOPMENT_BIN ]]; then
    printf 'Fetching temporary Sanoid development ref %s...\n' "$SANOID_DEVELOPMENT_REF"
    git -c advice.detachedHead=false clone --quiet --depth 1 --single-branch --branch "$SANOID_DEVELOPMENT_REF" \
        "$SANOID_REPOSITORY" "$DEPENDENCY_DIR/development" || die "could not fetch Sanoid development"
    SANOID_DEVELOPMENT_BIN=$DEPENDENCY_DIR/development/sanoid
    SANOID_DEVELOPMENT_DEFAULTS=$DEPENDENCY_DIR/development/sanoid.defaults.conf
    SANOID_DEVELOPMENT_REVISION=$(git -C "$DEPENDENCY_DIR/development" rev-parse --short HEAD)
fi

SANOID_STABLE_BIN="$SANOID_STABLE_BIN" \
SANOID_STABLE_DEFAULTS="$SANOID_STABLE_DEFAULTS" \
SANOID_DEVELOPMENT_BIN="$SANOID_DEVELOPMENT_BIN" \
SANOID_DEVELOPMENT_DEFAULTS="$SANOID_DEVELOPMENT_DEFAULTS" \
SANOID_DEVELOPMENT_REVISION="$SANOID_DEVELOPMENT_REVISION" \
    "$SCRIPT_DIR/zfs-benchmark.sh" "$@"

printf 'Benchmark command completed; reports are under %s.\n' "$SCRIPT_DIR"
