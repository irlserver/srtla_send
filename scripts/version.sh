#!/usr/bin/env bash
#
# Print the srtla_send version from the [package] table of Cargo.toml.
#
# Read with sed instead of `cargo metadata` so the release workflow can check a
# tag against it without first installing the pinned toolchain.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

version="$(sed -nE '/^\[package\]/,/^\[/ s/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "${root}/Cargo.toml" | head -n1)"

if [[ -z ${version} ]]; then
	echo "Cargo.toml: no version found in the [package] table" >&2
	exit 1
fi

printf '%s\n' "${version}"
