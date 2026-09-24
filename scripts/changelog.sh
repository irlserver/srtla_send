#!/usr/bin/env bash
#
# Build release notes from the conventional commits in a tag range.
#
# GitHub's generate-notes only sees pull requests, and much of the work here
# lands as direct commits. Every commit subject is a conventional commit, which
# is enough to group a tag range into a readable changelog.
#
# Usage: scripts/changelog.sh <tag> [previous-tag]
#
#   scripts/changelog.sh v4.1.0          # notes for a tag that already exists
#   scripts/changelog.sh HEAD            # preview what the next tag would say
#
# The previous tag defaults to the nearest v* tag reachable from <tag>'s
# parent. Markdown goes to stdout.

set -euo pipefail

tag="${1:?usage: changelog.sh <tag> [previous-tag]}"
prev="${2:-}"

git rev-parse --verify --quiet "${tag}^{commit}" >/dev/null || {
	echo "no such commit-ish: ${tag}" >&2
	exit 1
}

if [[ -z ${prev} ]]; then
	prev="$(git describe --tags --abbrev=0 --match 'v[0-9]*' "${tag}^" 2>/dev/null || true)"
fi

if [[ -n ${prev} ]]; then
	range="${prev}..${tag}"
else
	range="${tag}"
fi

# Commit and compare links need the repository URL. Actions provides it;
# locally it comes from the origin remote in either SSH or HTTPS form.
if [[ -n ${GITHUB_REPOSITORY:-} ]]; then
	repo_url="${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY}"
else
	origin="$(git remote get-url origin 2>/dev/null || true)"
	origin="${origin%.git}"
	case "${origin}" in
	# scp-style is git@host:owner/repo, so the colon is the path separator.
	git@*) host_path="${origin#git@}" && repo_url="https://${host_path/://}" ;;
	ssh://git@*) repo_url="https://${origin#ssh://git@}" ;;
	http*) repo_url="${origin}" ;;
	*) repo_url="" ;;
	esac
fi

# One file per section, because macOS still ships bash 3.2 and has no
# associative arrays.
sections="$(mktemp -d)"
trap 'rm -rf "${sections}"' EXIT

# shellcheck disable=SC2016  # entries are Markdown, backticks are literal
entry_line() {
	local scope="$1" subject="$2" sha="$3" short="${3:0:7}" line=""

	# Most commits are scoped to the binary itself, so that scope adds nothing.
	if [[ -n ${scope} && ${scope} != "srtla_send" ]]; then
		line="**${scope}:** "
	fi
	line+="${subject}"
	if [[ -n ${repo_url} ]]; then
		line+=" ([\`${short}\`](${repo_url}/commit/${sha}))"
	else
		line+=" (\`${short}\`)"
	fi
	printf -- '- %s\n' "${line}"
}

# %x1f separates fields and %x1e separates commits, so a multi-line body stays
# attached to the commit it came from.
while IFS=$'\x1f' read -r -d $'\x1e' sha subject body; do
	sha="${sha#$'\n'}"
	[[ -n ${sha} ]] || continue

	type="" scope="" bang="" text="${subject}"
	if [[ ${subject} =~ ^([A-Za-z]+)(\(([^\)]*)\))?(!)?:[[:space:]]+(.*)$ ]]; then
		type="$(printf '%s' "${BASH_REMATCH[1]}" | tr '[:upper:]' '[:lower:]')"
		scope="${BASH_REMATCH[3]}"
		bang="${BASH_REMATCH[4]}"
		text="${BASH_REMATCH[5]}"
	fi

	if [[ -n ${bang} || ${body} == *"BREAKING CHANGE:"* ]]; then
		entry_line "${scope}" "${text}" "${sha}" >>"${sections}/breaking"
		# The text after the marker is the migration note, the one part of a
		# commit message a reader needs inline.
		printf '%s\n' "${body}" |
			sed -n 's/^BREAKING CHANGE:[[:space:]]*/  - /p' >>"${sections}/breaking"
		continue
	fi

	case "${type}" in
	feat) name=features ;;
	fix) name=fixes ;;
	perf) name=perf ;;
	refactor) name=refactor ;;
	docs) name=docs ;;
	build | ci) name=build ;;
	*) name=other ;;
	esac
	entry_line "${scope}" "${text}" "${sha}" >>"${sections}/${name}"
done < <(git log --no-merges --format="%H%x1f%s%x1f%b%x1e" "${range}")

wrote=0
section() {
	[[ -s ${sections}/$2 ]] || return 0
	printf '### %s\n\n' "$1"
	cat "${sections}/$2"
	printf '\n'
	wrote=1
}

section "Breaking changes" breaking
section "Features" features
section "Bug fixes" fixes
section "Performance" perf
section "Refactoring" refactor
section "Documentation" docs
section "Build and CI" build
section "Other changes" other

if [[ ${wrote} -eq 0 ]]; then
	printf 'No changes recorded for this release.\n\n'
fi

if [[ -n ${repo_url} && -n ${prev} ]]; then
	printf '**Full changelog**: %s/compare/%s...%s\n' "${repo_url}" "${prev}" "${tag}"
fi
