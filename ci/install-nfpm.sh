#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 VERSION ARCH DESTINATION" >&2
  exit 2
fi

version=$1
arch=$2
destination=$3

case "$version" in
  ""|*[!0-9A-Za-z._-]*)
    echo "invalid nFPM version: $version" >&2
    exit 2
    ;;
esac

case "$arch" in
  x86_64|arm64) ;;
  *)
    echo "unsupported nFPM architecture: $arch" >&2
    exit 2
    ;;
esac

for command_name in curl sha256sum tar install mktemp; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "required command not found: $command_name" >&2
    exit 1
  fi
done

temporary=$(mktemp -d "${TMPDIR:-/tmp}/zsnap-nfpm.XXXXXX")
trap 'rm -rf "$temporary"' 0 HUP INT TERM

archive="nfpm_${version}_Linux_${arch}.tar.gz"
base="https://github.com/goreleaser/nfpm/releases/download/v${version}"

curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "$temporary/checksums.txt" "$base/checksums.txt"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "$temporary/$archive" "$base/$archive"

expected=$(awk -v archive="$archive" '$2 == archive { print; found = 1 } END { if (!found) exit 1 }' \
  "$temporary/checksums.txt") || {
  echo "checksum for $archive is missing" >&2
  exit 1
}
printf '%s\n' "$expected" | (
  cd "$temporary"
  sha256sum --check --strict -
)

tar -xzf "$temporary/$archive" -C "$temporary" nfpm
install -d -m 0755 "$destination"
install -m 0755 "$temporary/nfpm" "$destination/nfpm"
"$destination/nfpm" --version
