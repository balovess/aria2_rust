#!/usr/bin/env bash
# sync-version.sh — sanity check that all workspace member crates inherit the
# workspace version via `version.workspace = true`.
#
# This script does NOT rewrite Cargo.toml files. It only verifies that:
#   1. The root Cargo.toml declares a `[workspace.package]` version.
#   2. Every member crate listed in `[workspace] members` has
#      `version.workspace = true` in its `[package]` section.
#
# Exit codes:
#   0 — all member crates correctly reference the workspace version.
#   1 — at least one crate has a hardcoded `version = "..."` instead of
#       `version.workspace = true`, or the workspace version is missing.
#
# Intended to be invoked from the release workflow (`release.yml`) before
# publishing, to fail fast on version drift.

set -euo pipefail

# Resolve the repository root from the script location so the script can be
# invoked from any working directory (CI, local, etc.).
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"

root_cargo="${repo_root}/Cargo.toml"

if [[ ! -f "${root_cargo}" ]]; then
    echo "ERROR: root Cargo.toml not found at ${root_cargo}" >&2
    exit 1
fi

# Extract the workspace version from the `[workspace.package]` section.
# We look for a line of the form `version = "x.y.z"` that appears after the
# `[workspace.package]` header and before any other section header.
workspace_version="$(
    awk '
        /^\[workspace\.package\]/ { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && /^[[:space:]]*version[[:space:]]*=/ {
            # Strip leading whitespace, the `version =` prefix, surrounding quotes.
            sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*/, "", $0)
            gsub(/^"|"$/, "", $0)
            print
            exit
        }
    ' "${root_cargo}"
)"

if [[ -z "${workspace_version}" ]]; then
    echo "ERROR: could not find \`version\` under [workspace.package] in ${root_cargo}" >&2
    exit 1
fi

echo "Workspace version: ${workspace_version}"

# Extract the member crate directory list from `[workspace] members`.
# Supports both inline arrays and multi-line arrays:
#   members = ["a", "b"]
#   members = [
#       "a",
#       "b",
#   ]
member_paths="$(
    awk '
        /^\[workspace\]/ { in_workspace = 1; next }
        /^\[/ { in_workspace = 0 }
        in_workspace && /^[[:space:]]*members[[:space:]]*=/ { in_members = 1 }
        in_members && /\]/ { in_members = 0 }
        in_members {
            # Strip everything up to and including `members =`, plus brackets,
            # commas, and surrounding double-quotes from each member path.
            sub(/^[[:space:]]*members[[:space:]]*=[[:space:]]*/, "", $0)
            gsub(/[\[\],"]/, "", $0)
            # Trim whitespace.
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
            if ($0 != "") print
        }
    ' "${root_cargo}"
)"

if [[ -z "${member_paths}" ]]; then
    echo "ERROR: no workspace members found in ${root_cargo}" >&2
    exit 1
fi

exit_code=0

while IFS= read -r member; do
    # Skip blank lines (defensive — awk already filters them).
    [[ -z "${member}" ]] && continue

    member_cargo="${repo_root}/${member}/Cargo.toml"

    if [[ ! -f "${member_cargo}" ]]; then
        echo "ERROR: member Cargo.toml not found: ${member_cargo}" >&2
        exit_code=1
        continue
    fi

    # Look for a `version` key in the `[package]` section of the member crate.
    # We accept only `version.workspace = true`; any hardcoded version is a failure.
    # Match `version` followed by `=` (hardcoded) or `.` (workspace reference).
    version_line="$(
        awk '
            /^\[package\]/ { in_package = 1; next }
            /^\[/ { in_package = 0 }
            in_package && /^[[:space:]]*version[[:space:]]*[=.]/ {
                print
                exit
            }
        ' "${member_cargo}"
    )"

    if [[ -z "${version_line}" ]]; then
        echo "ERROR: ${member}/Cargo.toml has no \`version\` field in [package]" >&2
        exit_code=1
        continue
    fi

    # Normalize for comparison: strip whitespace.
    normalized="$(echo "${version_line}" | tr -d '[:space:]')"

    if [[ "${normalized}" == "version.workspace=true" ]]; then
        echo "OK: ${member}/Cargo.toml -> version.workspace = true"
    else
        echo "ERROR: ${member}/Cargo.toml has hardcoded version instead of \`version.workspace = true\`:" >&2
        echo "       ${version_line}" >&2
        exit_code=1
    fi
done <<< "${member_paths}"

if [[ ${exit_code} -eq 0 ]]; then
    echo "All member crates correctly reference workspace version ${workspace_version}."
fi

exit "${exit_code}"
