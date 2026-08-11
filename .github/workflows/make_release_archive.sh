#!/usr/bin/env bash
# Builds a `rules_oci_runtime` archive from a commit, stamped with the version
# and with a launcher for each platform: either pinned to the release assets it
# will be published alongside, or baked in so the archive stands alone.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: make_release_archive.sh --version VERSION --output PATH \
                              [--commit COMMITISH] \
                              ( --pin-amd64 SHA256 --pin-arm64 SHA256 |
                                --bake-amd64 PATH --bake-arm64 PATH )

A pinned archive downloads the launcher from the release assets of the version
it is stamped with, so it only works once that release exists. A baked archive
carries the launcher, so it can be tried out before one is cut.

Only tracked files at COMMIT (default HEAD) end up in the archive.
EOF
    exit 2
}

version=""
pin_amd64=""
pin_arm64=""
bake_amd64=""
bake_arm64=""
output=""
commit="HEAD"

while (($#)); do
    case "$1" in
    --version) version="${2:-}" && shift 2 ;;
    --pin-amd64) pin_amd64="${2:-}" && shift 2 ;;
    --pin-arm64) pin_arm64="${2:-}" && shift 2 ;;
    --bake-amd64) bake_amd64="${2:-}" && shift 2 ;;
    --bake-arm64) bake_arm64="${2:-}" && shift 2 ;;
    --output) output="${2:-}" && shift 2 ;;
    --commit) commit="${2:-}" && shift 2 ;;
    *) usage ;;
    esac
done

[[ -n "${version}" && -n "${output}" ]] || usage

if [[ -n "${pin_amd64}" && -n "${pin_arm64}" && -z "${bake_amd64}${bake_arm64}" ]]; then
    pinned=true
elif [[ -n "${bake_amd64}" && -n "${bake_arm64}" && -z "${pin_amd64}${pin_arm64}" ]]; then
    pinned=false
else
    usage
fi

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
    "${root}/.bazelignore" \
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
    --expression="0,/^    version = \".*\",\$/s||    version = \"${version}\",|" \
    "${root}/MODULE.bazel"

# The substitutions silently do nothing if a file is restructured.
assert_contains() {
    grep --quiet --fixed-strings --line-regexp -- "$2" "${root}/$1" ||
        { echo "make_release_archive.sh: ${1} is missing '${2}', has it been restructured?" >&2 && exit 1; }
}
assert_contains MODULE.bazel "    version = \"${version}\","

if "${pinned}"; then
    sed --in-place \
        --expression="s|^LAUNCHER_VERSION = .*|LAUNCHER_VERSION = \"${version}\"|" \
        --expression="s|^    \"linux_amd64\": .*|    \"linux_amd64\": \"${pin_amd64}\",|" \
        --expression="s|^    \"linux_arm64\": .*|    \"linux_arm64\": \"${pin_arm64}\",|" \
        "${root}/lib/private/versions.bzl"

    assert_contains lib/private/versions.bzl "LAUNCHER_VERSION = \"${version}\""
    assert_contains lib/private/versions.bzl "    \"linux_amd64\": \"${pin_amd64}\","
    assert_contains lib/private/versions.bzl "    \"linux_arm64\": \"${pin_arm64}\","
else
    # `//launcher` globs these, and declares a toolchain for each one found.
    install -m 0755 "${bake_amd64}" "${root}/launcher/oci_runtime.amd64"
    install -m 0755 "${bake_arm64}" "${root}/launcher/oci_runtime.arm64"
fi

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
