#!/usr/bin/env bash
#
# Package a compiled srtla_send binary as the srtla .deb.
#
# Usage: scripts/package-deb.sh <binary> <deb-arch> <glibc-floor> <out-dir>
#
#   scripts/package-deb.sh target/aarch64-unknown-linux-gnu/release-lto/srtla_send arm64 2.27 dist
#
# The control file promises `libc6 (>= <glibc-floor>)`. Before it writes that
# promise, the script reads the glibc symbol versions the binary links against
# and refuses to package it if any of them is newer than the floor. A binary
# built on a new runner without cargo-zigbuild's glibc pin fails here, not on
# a user's device.

set -euo pipefail

usage="usage: package-deb.sh <binary> <deb-arch> <glibc-floor> <out-dir>"
bin="${1:?${usage}}"
arch="${2:?${usage}}"
floor="${3:?${usage}}"
out="${4:?${usage}}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$("${root}/scripts/version.sh")"

# readelf, unlike objdump, reads any ELF architecture, so the x86 runner can
# inspect the arm64 binary.
needed="$(readelf --version-info --wide "${bin}" |
	grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n1)"
if [[ -z ${needed} ]]; then
	echo "${bin}: no versioned glibc symbols found; is this a glibc binary?" >&2
	exit 1
fi
if [[ "$(printf '%s\n%s\n' "${needed}" "${floor}" | sort -V | tail -n1)" != "${floor}" ]]; then
	echo "${bin} needs glibc ${needed}, newer than the ${floor} floor the package promises." >&2
	echo "Build it with cargo zigbuild --target <triple>.${floor}." >&2
	exit 1
fi
echo "  ok    ${bin} needs glibc ${needed} (floor ${floor})"

staging="$(mktemp -d)"
trap 'rm -rf "${staging}"' EXIT
# mktemp -d creates 0700, and the staging root becomes the package's `./`.
chmod 755 "${staging}"

install -Dm755 "${bin}" "${staging}/usr/bin/srtla_send"
mkdir -p "${staging}/DEBIAN"
cat >"${staging}/DEBIAN/control" <<EOF
Package: srtla
Version: ${version}
Architecture: ${arch}
Maintainer: Thomas Lekanger <mail@datagutt.no>
Description: SRT transport proxy with link aggregation for connection bonding
Depends: libc6 (>= ${floor})
Homepage: https://irlserver.com
Section: net
Priority: optional
EOF

mkdir -p "${out}"
deb="${out}/srtla_${version}_${arch}.deb"
# CI builds as an unprivileged runner user; without --root-owner-group the
# installed binary would belong to that uid.
dpkg-deb --root-owner-group --build "${staging}" "${deb}"
echo "  ok    ${deb}"
