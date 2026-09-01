#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 TARGET VERSION PACKAGE_ARCH" >&2
  exit 2
fi

target=$1
version=$2
package_arch=$3
nfpm=${NFPM:-nfpm}
dist_dir=${DIST_DIR:-dist}

case "$target" in
  x86_64-unknown-linux-musl)
    expected_arch=amd64
    deb_arch=amd64
    native_arch=x86_64
    ;;
  aarch64-unknown-linux-musl)
    expected_arch=arm64
    deb_arch=arm64
    native_arch=aarch64
    ;;
  *)
    echo "unsupported package target: $target" >&2
    exit 2
    ;;
esac

if [ "$package_arch" != "$expected_arch" ]; then
  echo "package architecture $package_arch does not match $target ($expected_arch)" >&2
  exit 2
fi

case "$version" in
  ""|*[!A-Za-z0-9._-]*)
    echo "invalid package version: $version" >&2
    exit 2
    ;;
esac

binary="target/$target/release/zsnap"
if [ ! -x "$binary" ]; then
  echo "release binary is missing or not executable: $binary" >&2
  exit 1
fi
if ! command -v "$nfpm" >/dev/null 2>&1; then
  echo "nFPM executable not found: $nfpm" >&2
  exit 1
fi

mkdir -p "$dist_dir"
export PACKAGE_ARCH="$package_arch"
export PACKAGE_BINARY="$binary"
export PACKAGE_VERSION="$version"

deb="$dist_dir/zsnap_${version}_${deb_arch}.deb"
rpm="$dist_dir/zsnap-${version}-1.${native_arch}.rpm"
apk="$dist_dir/zsnap-${version}-${native_arch}.apk"
archlinux="$dist_dir/zsnap-${version}-1-${native_arch}.pkg.tar.zst"

for artifact in "$deb" "$rpm" "$apk" "$archlinux"; do
  if [ -e "$artifact" ] || [ -e "$artifact.sha256" ]; then
    echo "refusing to overwrite an existing package artifact: $artifact" >&2
    exit 1
  fi
done

"$nfpm" package --config packaging/nfpm.yaml --packager deb --target "$deb"
"$nfpm" package --config packaging/nfpm.yaml --packager rpm --target "$rpm"
"$nfpm" package --config packaging/nfpm.yaml --packager apk --target "$apk"
"$nfpm" package --config packaging/nfpm.yaml --packager archlinux --target "$archlinux"

for artifact in "$deb" "$rpm" "$apk" "$archlinux"; do
  artifact_name=${artifact##*/}
  (
    cd "$dist_dir"
    sha256sum "$artifact_name" > "$artifact_name.sha256"
  )
  echo "created $artifact"
  echo "created $artifact.sha256"
done
