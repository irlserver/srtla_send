#!/usr/bin/env bash
#
# build-deb.sh — assemble a pipeline-compatible Debian package for srtla-send-rs.
#
# This is the SINGLE source of truth for the .deb control metadata and the
# artifact filename. Both the CI build matrix (.github/workflows/ci.yml) and the
# tag-triggered release (.github/workflows/release.yml) call it, so the package
# name, binary install path, Conflicts declaration and filename can never drift
# between "build on every push" and "build on tag".
#
# Why the artifact name matters: the device image builder fetches first-party
# .debs with the glob `*${ARCH}*.deb` (image-building-pipeline
# v2/lib/fetch-debs.sh, ARCH in {arm64, amd64}). The emitted name
#   srtla-send-rs_<version>_<arch>.deb
# matches that glob. This script re-runs the EXACT glob against the real
# filename as a self-test so a future rename cannot silently break the fetch.
#
# Why Conflicts: srtla — historically the srtla package shipped BOTH
# /usr/bin/srtla_send (C sender) and /usr/bin/srtla_rec. srtla-send-rs installs
# /usr/bin/srtla_send, so it file-conflicts with every srtla that still ships the
# C sender. At cutover (ADR-003) srtla dropped srtla_send from its .deb; from that
# release on the two packages coexist (srtla = receiver only, srtla-send-rs =
# sender). The bound below is that cutover version: srtla (<< cutover) conflicts;
# srtla (>= cutover) does not. The cutover landed in srtla v2026.6.2 (the first
# receiver-only release — CMakeLists installs srtla_rec only), so the default is
# 2026.6.2: srtla 2026.6.1 and earlier conflict (they ship the C sender), 2026.6.2
# and later coexist.
#
# Usage:
#   ci/build-deb.sh <arch> <path-to-srtla_send-binary> [out-dir]
#     <arch>  arm64 | amd64   (Debian architecture, matches fetch-debs.sh ARCH)
#
# Env:
#   SRTLA_CUTOVER_VERSION   srtla version that retires the C sender (default 2026.6.2)
#
# Without dpkg-deb on PATH (non-Debian dev hosts) the script still assembles the
# package tree, writes + prints the control file, computes the filename and runs
# the glob self-test, then skips the final `dpkg-deb --build`. On a Debian/Ubuntu
# CI runner it builds the .deb and asserts its metadata + contents.

set -euo pipefail

die() { printf 'build-deb: ERROR: %s\n' "$*" >&2; exit 1; }
info() { printf 'build-deb: %s\n' "$*" >&2; }

ARCH="${1:-}"
BIN="${2:-}"
OUTDIR="${3:-.}"

[[ -n "${ARCH}" ]] || die "missing <arch> (arm64|amd64)"
[[ -n "${BIN}" ]]  || die "missing <path-to-srtla_send-binary>"
case "${ARCH}" in
  arm64|amd64) ;;
  *) die "unsupported arch '${ARCH}' (expected arm64 or amd64)" ;;
esac
[[ -f "${BIN}" ]] || die "binary not found: ${BIN}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/.." && pwd)"
CARGO_TOML="${REPO_ROOT}/Cargo.toml"
[[ -f "${CARGO_TOML}" ]] || die "Cargo.toml not found at ${CARGO_TOML}"

# Package version comes from the crate version (the binary that is shipped).
VERSION="$(awk -F\" '/^version = /{print $2; exit}' "${CARGO_TOML}")"
[[ -n "${VERSION}" ]] || die "could not read version from ${CARGO_TOML}"

CUTOVER="${SRTLA_CUTOVER_VERSION:-2026.6.2}"

OUT="${OUTDIR%/}/srtla-send-rs_${VERSION}_${ARCH}.deb"

info "version=${VERSION} arch=${ARCH} cutover=${CUTOVER}"
info "binary=${BIN}"
info "output=${OUT}"

# ---------------------------------------------------------------------------
# Assemble the package tree. install -D creates the parent dirs; the binary is
# placed at exactly /usr/bin/srtla_send (the parity-contract binary name).
# ---------------------------------------------------------------------------
STAGE="$(mktemp -d)"
trap 'rm -rf "${STAGE}"' EXIT
PKGROOT="${STAGE}/pkg"

install -D -m 0755 "${BIN}" "${PKGROOT}/usr/bin/srtla_send"

INSTALLED_KB="$(du -k -s "${PKGROOT}/usr" | cut -f1)"

mkdir -p "${PKGROOT}/DEBIAN"
cat > "${PKGROOT}/DEBIAN/control" <<EOF
Package: srtla-send-rs
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: CERALIVE <contact@ceralive.com>
Installed-Size: ${INSTALLED_KB}
Depends: libc6
Conflicts: srtla (<< ${CUTOVER})
Replaces: srtla (<< ${CUTOVER})
Section: net
Priority: optional
Homepage: https://github.com/CERALIVE/srtla-send-rs
Description: SRTLA bonding sender (Rust) — CERALIVE fork of irlserver/srtla_send
 Reads local SRT (UDP) on a listen port and forwards it over multiple bonded
 uplinks to an SRTLA receiver, balancing by link capacity and quality. Installs
 the srtla_send binary. This is CERALIVE's device-side streaming sender; it
 supersedes the C srtla_send shipped by the srtla package (see Conflicts).
EOF

info "control file:"
sed 's/^/  | /' "${PKGROOT}/DEBIAN/control" >&2

# ---------------------------------------------------------------------------
# Filename self-test: re-run the EXACT glob image-building-pipeline uses
# (`*${ARCH}*.deb`) against the real artifact name. A rename that breaks the
# device fetch fails the build here, not silently at image-build time.
# ---------------------------------------------------------------------------
base="$(basename "${OUT}")"
glob="*${ARCH}*.deb"
# shellcheck disable=SC2254  # intentional glob match, ARCH is validated above
case "${base}" in
  $glob) info "glob self-test OK: '${base}' matches fetch-debs.sh pattern '${glob}'" ;;
  *) die "glob self-test FAILED: '${base}' does not match '${glob}'" ;;
esac

# ---------------------------------------------------------------------------
# Build the .deb when dpkg-deb is available, then assert metadata + contents.
# --root-owner-group keeps ownership reproducible (root:root) without fakeroot.
# ---------------------------------------------------------------------------
if command -v dpkg-deb >/dev/null 2>&1; then
  mkdir -p "${OUTDIR}"
  dpkg-deb --root-owner-group --build "${PKGROOT}" "${OUT}"

  info "dpkg-deb --info ${OUT}:"
  dpkg-deb --info "${OUT}" | sed 's/^/  | /' >&2
  info "dpkg-deb --contents ${OUT}:"
  dpkg-deb --contents "${OUT}" | sed 's/^/  | /' >&2

  # Hard assertions on the expected-outcome contract.
  dpkg-deb -f "${OUT}" Package      | grep -qx 'srtla-send-rs'   || die "Package field != srtla-send-rs"
  dpkg-deb -f "${OUT}" Architecture | grep -qx "${ARCH}"         || die "Architecture field != ${ARCH}"
  dpkg-deb -f "${OUT}" Conflicts    | grep -q  'srtla (<< '      || die "Conflicts: srtla (<< …) missing"
  dpkg-deb --contents "${OUT}" | grep -q ' \./usr/bin/srtla_send$' \
    || die "/usr/bin/srtla_send missing from package contents"

  info "all .deb assertions passed -> ${OUT}"
else
  info "dpkg-deb not found — assembled + validated the package tree and filename;"
  info "skipping the final 'dpkg-deb --build' (run on a Debian/Ubuntu CI runner)."
fi
