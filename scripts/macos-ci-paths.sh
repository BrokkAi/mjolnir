#!/usr/bin/env bash
set -euo pipefail

# Decide whether a change should start the macOS CI lane right away, so the
# lane skips the many changes that cannot affect macOS.
#
# Usage: bash scripts/macos-ci-paths.sh BASE HEAD
# Run from inside the git work tree under test. BASE and HEAD are any
# revisions: a SHA, HEAD^1, HEAD.
#
# Output contract: one reason per line on stderr, then exactly true or false
# on stdout, always with exit status 0. The exit status is non-zero only for a
# usage error or an unreadable pattern file.
#
# Uncertainty biases to true. An unreachable base revision, an unreachable
# head revision, or a failing git command all report true, because a missed
# macOS run is worse than an extra one.

if [[ $# -ne 2 ]]; then
    printf 'usage: %s BASE HEAD\n' "${BASH_SOURCE[0]}" >&2
    exit 2
fi

# The pattern list lives at the repository root next to this script, not next
# to the cwd, so the test can run this script inside a temporary repository.
repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
patterns_file="$repository_root/.github/macos-ci-paths.txt"

if [[ ! -r "$patterns_file" ]]; then
    printf 'cannot read pattern file %s\n' "$patterns_file" >&2
    exit 1
fi

# Report a reason and the decision, then stop.
decide() {
    printf '%s\n' "$1" >&2
    printf '%s\n' "$2"
    exit 0
}

# Print the commit named by a revision, or fail for an unusable revision.
resolve_revision() {
    local revision="$1"
    # GitHub sends an all-zero `before` for a new branch; there is nothing to
    # diff against in that case.
    if [[ -z "$revision" || "$revision" =~ ^0+$ ]]; then
        return 1
    fi
    git rev-parse --verify --quiet "$revision^{commit}"
}

base_argument="$1"
head_argument="$2"

if ! base_sha="$(resolve_revision "$base_argument")"; then
    decide "cannot resolve base revision '$base_argument'; running macOS" true
fi
if ! head_sha="$(resolve_revision "$head_argument")"; then
    decide "cannot resolve head revision '$head_argument'; running macOS" true
fi

# --no-renames keeps a move out of a listed directory visible as the old path.
# -z writes raw path bytes, so git does not C-quote a name that holds
# non-ASCII or newline bytes into a quoted string that matches no pattern.
if ! diff_file="$(mktemp)"; then
    decide 'cannot create a temporary file for git diff; running macOS' true
fi
trap 'rm -f "$diff_file"' EXIT

if ! git diff -z --name-only --no-renames "$base_sha" "$head_sha" >"$diff_file"; then
    decide "git diff failed for '$base_argument'..'$head_argument'; running macOS" true
fi

paths=()
mapfile -d '' -t paths <"$diff_file"
if [[ ${#paths[@]} -eq 0 ]]; then
    decide 'no changed paths' false
fi

patterns=()
while IFS= read -r line; do
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    if [[ -z "$line" || "$line" == '#'* ]]; then
        continue
    fi
    patterns+=("$line")
done <"$patterns_file"

matched=false

for path in "${paths[@]}"; do
    for pattern in "${patterns[@]}"; do
        # The pattern side stays unquoted: these are glob patterns, and `*`
        # matches `/`.
        # shellcheck disable=SC2053
        if [[ $path == $pattern ]]; then
            printf '%s matches %s\n' "$path" "$pattern" >&2
            matched=true
            break
        fi
    done
done

# A macOS cfg gate anywhere in the old or new content of a changed file means
# the change can alter macOS behavior even when the path is not listed.
# `not(target_os = "linux")` selects code that macOS runs and Linux CI never
# compiles, so it counts as a macOS gate too.
content_marker='target_os[[:space:]]*=[[:space:]]*"macos"|target_vendor[[:space:]]*=[[:space:]]*"apple"|not\(target_os[[:space:]]*=[[:space:]]*"linux"\)'

scan_tree() {
    local revision="$1"
    local label="$2"
    local status=0
    local scan_output line
    # quotePath=false keeps the reported names readable instead of C-quoted.
    scan_output="$(git -c core.quotePath=false --literal-pathspecs grep -l -I -E "$content_marker" "$revision" -- "${paths[@]}")" || status=$?
    # git grep exits 0 on a match and 1 on no match. Anything above that is an
    # error, and an error means we cannot tell.
    if ((status > 1)); then
        decide "git grep failed in the $label tree; running macOS" true
    fi
    while IFS= read -r line; do
        if [[ -n "$line" ]]; then
            printf '%s contains a macOS cfg gate (%s)\n' "${line#"$revision:"}" "$label" >&2
            matched=true
        fi
    done <<<"$scan_output"
}

# The base tree search also covers files this change deleted.
scan_tree "$base_sha" base
scan_tree "$head_sha" head

if [[ "$matched" == false ]]; then
    decide "no macOS-sensitive paths among ${#paths[@]} changed paths" false
fi

printf 'true\n'
