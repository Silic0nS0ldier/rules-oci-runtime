#!/usr/bin/env bash
# Builds the `rules_oci_runtime` release archive from a commit, baking the
# release version and the launcher binary hashes into `lib/private/versions.bzl`
# so consumers get a working prebuilt launcher toolchain.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: make_release_archive.sh --version VERSION --amd64 SHA256 --arm64 SHA256 \
                               --output PATH [--commit COMMITISH]

Only tracked files at COMMIT (default HEAD) end up in the archive.
EOF
    exit 2
}

version=""
amd64=""
arm64=""
output=""
commit="HEAD"

while (($#)); do
    case "$1" in
    --version) version="${2:-}" && shift 2 ;;
    --amd64) amd64="${2:-}" && shift 2 ;;
    --arm64) arm64="${2:-}" && shift 2 ;;
    --output) output="${2:-}" && shift 2 ;;
    --commit) commit="${2:-}" && shift 2 ;;
    *) usage ;;
    esac
done

[[ -n "${version}" && -n "${amd64}" && -n "${arm64}" && -n "${output}" ]] || usage

repo_root="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
prefix="rules_oci_runtime-${version}"
stage="$(mktemp -d)"
trap 'rm -rf "${stage}"' EXIT

git -C "${repo_root}" archive --format=tar --prefix="${prefix}/" "${commit}" |
    tar --extract --directory="${stage}"
root="${stage}/${prefix}"

# Development only, and `docs/BUILD.bazel` would drag in the `@stardoc` dev
# dependency that consumers never fetch.
rm -rf \
    "${root}/.bazelrc" \
    "${root}/.bazelversion" \
    "${root}/.devcontainer" \
    "${root}/.github" \
    "${root}/.gitignore" \
    "${root}/MODULE.bazel.lock" \
    "${root}/REPO.bazel" \
    "${root}/docs/BUILD.bazel" \
    "${root}/e2e" \
    "${root}/source"

sed --in-place \
    --expression="s|^LAUNCHER_VERSION = .*|LAUNCHER_VERSION = \"${version}\"|" \
    --expression="s|^    \"linux_amd64\": .*|    \"linux_amd64\": \"${amd64}\",|" \
    --expression="s|^    \"linux_arm64\": .*|    \"linux_arm64\": \"${arm64}\",|" \
    "${root}/lib/private/versions.bzl"

sed --in-place \
    --expression="0,/^    version = \".*\",\$/s||    version = \"${version}\",|" \
    "${root}/MODULE.bazel"

# The substitutions above silently do nothing if either file is restructured.
assert_contains() {
    grep --quiet --fixed-strings --line-regexp -- "$2" "${root}/$1" ||
        { echo "make_release_archive.sh: ${1} is missing '${2}', has it been restructured?" >&2 && exit 1; }
}
assert_contains lib/private/versions.bzl "LAUNCHER_VERSION = \"${version}\""
assert_contains lib/private/versions.bzl "    \"linux_amd64\": \"${amd64}\","
assert_contains lib/private/versions.bzl "    \"linux_arm64\": \"${arm64}\","
assert_contains MODULE.bazel "    version = \"${version}\","

# Reproducible: the same commit and inputs always produce the same bytes.
tar --create \
    --directory="${stage}" \
    --sort=name \
    --mtime="@0" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --format=gnu \
    "${prefix}" |
    gzip --no-name >"${output}"

echo "Wrote ${output}"
