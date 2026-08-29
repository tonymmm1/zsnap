#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 TARGET VERSION" >&2
  exit 2
fi

target=$1
version=$2

case "$target" in
  ""|*[!A-Za-z0-9._-]*)
    echo "invalid release target: $target" >&2
    exit 2
    ;;
esac

case "$version" in
  ""|*[!A-Za-z0-9._-]*)
    echo "invalid release version: $version" >&2
    exit 2
    ;;
esac

binary="target/$target/release/zsnap"
if [ ! -x "$binary" ]; then
  echo "release binary is missing or not executable: $binary" >&2
  exit 1
fi

dist_dir=${DIST_DIR:-dist}
archive_base="zsnap-$version-$target"
archive="$dist_dir/$archive_base.tar.gz"
checksum="$archive.sha256"

mkdir -p "$dist_dir"
if [ -e "$archive" ] || [ -e "$checksum" ]; then
  echo "refusing to overwrite an existing release artifact: $archive" >&2
  exit 1
fi

staging=$(mktemp -d "${TMPDIR:-/tmp}/zsnap-release.XXXXXX")
cleanup() {
  rm -rf "$staging"
}
trap cleanup 0 HUP INT TERM

package_dir="$staging/$archive_base"
mkdir -p "$package_dir/assets" "$package_dir/contrib"
install -m 755 "$binary" "$package_dir/zsnap"
install -m 644 README.md LICENSE config.example.toml "$package_dir/"
install -m 644 assets/zsnap-logo.png "$package_dir/assets/"
install -m 644 \
  contrib/webhooks.env.example \
  contrib/zsnap.service \
  contrib/zsnap.timer \
  "$package_dir/contrib/"
install -m 755 \
  contrib/zsnap.openrc \
  contrib/zsnap.periodic \
  "$package_dir/contrib/"

tar -C "$staging" -czf "$archive" "$archive_base"
(
  cd "$dist_dir"
  sha256sum "$archive_base.tar.gz" > "$archive_base.tar.gz.sha256"
)

echo "created $archive"
echo "created $checksum"
