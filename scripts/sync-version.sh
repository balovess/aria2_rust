#!/usr/bin/env bash
# sync-version.sh - validate independent workspace member versions.
#
# This script does NOT rewrite Cargo.toml files. It verifies that every
# workspace member owns an explicit package version and reports the `aria2`
# binary version separately. Library versions are intentionally allowed to
# differ from one another and from the binary version.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
root_cargo="${repo_root}/Cargo.toml"

if [[ ! -f "${root_cargo}" ]]; then
    echo "ERROR: root Cargo.toml not found at ${root_cargo}" >&2
    exit 1
fi

member_paths="$(
    awk '
        /^\[workspace\]/ { in_workspace = 1; next }
        /^\[/ { in_workspace = 0 }
        in_workspace && /^[[:space:]]*members[[:space:]]*=/ { in_members = 1 }
        in_members && /\]/ { in_members = 0 }
        in_members {
            sub(/^[[:space:]]*members[[:space:]]*=[[:space:]]*/, "", $0)
            gsub(/[\[\],"]/, "", $0)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $0)
            if ($0 != "") print
        }
    ' "${root_cargo}"
)"

if [[ -z "${member_paths}" ]]; then
    echo "ERROR: no workspace members found in ${root_cargo}" >&2
    exit 1
fi

binary_version=""
exit_code=0

while IFS= read -r member; do
    [[ -z "${member}" ]] && continue

    member_cargo="${repo_root}/${member}/Cargo.toml"
    if [[ ! -f "${member_cargo}" ]]; then
        echo "ERROR: member Cargo.toml not found: ${member_cargo}" >&2
        exit_code=1
        continue
    fi

    version_line="$(
        awk '
            /^\[package\]/ { in_package = 1; next }
            /^\[/ { in_package = 0 }
            in_package && /^[[:space:]]*version[[:space:]]*=/ {
                print
                exit
            }
        ' "${member_cargo}"
    )"

    if [[ -z "${version_line}" ]]; then
        echo "ERROR: ${member}/Cargo.toml has no explicit version in [package]" >&2
        exit_code=1
        continue
    fi

    version="$(echo "${version_line}" | sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p')"
    if [[ -z "${version}" ]]; then
        echo "ERROR: ${member}/Cargo.toml must use version = \"...\", found: ${version_line}" >&2
        exit_code=1
        continue
    fi

    if [[ "${member}" == "aria2" ]]; then
        binary_version="${version}"
        echo "OK: aria2 binary release version -> ${version}"
    else
        echo "OK: ${member} library version -> ${version}"
    fi
done <<< "${member_paths}"

if [[ -z "${binary_version}" ]]; then
    echo "ERROR: workspace member 'aria2' is missing; binary release version is undefined" >&2
    exit_code=1
fi

if [[ ${exit_code} -eq 0 ]]; then
    echo "Independent crate versions are valid; binary releases use aria2 ${binary_version}."
fi

exit "${exit_code}"
