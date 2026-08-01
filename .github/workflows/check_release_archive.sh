#!/usr/bin/env bash
# Checks that a release archive works as a Bazel module: that it is
# self-contained, and optionally that a container runs with the launcher it pins
# or with one supplied locally. The archive may be a local path or a URL.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: check_release_archive.sh --version VERSION --archive PATH_OR_URL \
                               [--run-container] [--launcher PATH]

`--run-container` uses the launcher the archive pins, which is only downloadable
once released. Pass `--launcher` to register a locally built one instead, as the
release assets of an untagged commit do.
EOF
    exit 2
}

version=""
archive=""
launcher=""
run_container=false

while (($#)); do
    case "$1" in
    --version) version="${2:-}" && shift 2 ;;
    --archive) archive="${2:-}" && shift 2 ;;
    --launcher) launcher="${2:-}" && shift 2 ;;
    --run-container) run_container=true && shift ;;
    *) usage ;;
    esac
done

[[ -n "${version}" && -n "${archive}" ]] || usage

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

if [[ "${archive}" == http://* || "${archive}" == https://* ]]; then
    url="${archive}"
    archive="${work}/archive.tar.gz"
    curl --fail --location --silent --show-error --output "${archive}" "${url}"
else
    archive="$(realpath "${archive}")"
    url="file://${archive}"
fi

consumer="${work}/consumer"
mkdir "${consumer}"

cat >"${consumer}/MODULE.bazel" <<EOF
module(name = "release_archive_check")

bazel_dep(name = "rules_oci_runtime", version = "${version}")
archive_override(
    module_name = "rules_oci_runtime",
    integrity = "sha256-$(openssl dgst -binary -sha256 "${archive}" | base64)",
    strip_prefix = "rules_oci_runtime-${version}",
    urls = ["${url}"],
)
EOF

: >"${consumer}/BUILD.bazel"
echo "common --symlink_prefix=.bazel/" >"${consumer}/.bazelrc"

if "${run_container}"; then
    cat >>"${consumer}/MODULE.bazel" <<'EOF'

bazel_dep(name = "rules_oci", version = "2.3.0")

oci = use_extension("@rules_oci//oci:extensions.bzl", "oci")
oci.pull(
    name = "alpine",
    digest = "sha256:4b7ce07002c69e8f3d704a9c5d6fd3053be500b7f1c69fc0d80990c2ad8dd412",
    image = "docker.io/library/alpine",
    platforms = [
        "linux/amd64",
        "linux/arm64",
    ],
    tag = "3.22.2",
)
use_repo(oci, "alpine")
EOF

    cat >"${consumer}/BUILD.bazel" <<'EOF'
load("@rules_oci_runtime//lib:defs.bzl", "runc_binary")

runc_binary(
    name = "container",
    image = "@alpine",
)
EOF
fi

if [[ -n "${launcher}" ]]; then
    mkdir "${consumer}/launcher"
    install -m 0755 "${launcher}" "${consumer}/launcher/oci_runtime"

    # Toolchains registered by the root module win, so the pinned launcher is
    # never downloaded.
    cat >>"${consumer}/MODULE.bazel" <<'EOF'

register_toolchains("//launcher:toolchain")
EOF

    cat >"${consumer}/launcher/BUILD.bazel" <<'EOF'
load("@rules_oci_runtime//lib:defs.bzl", "launcher_toolchain")

launcher_toolchain(
    name = "launcher",
    binary = "oci_runtime",
)

toolchain(
    name = "toolchain",
    toolchain = ":launcher",
    toolchain_type = "@rules_oci_runtime//lib:launcher_toolchain_type",
)
EOF
fi

cd "${consumer}"

# Fails if the archive references anything left behind, such as the development
# only modules or the Stardoc targets.
bazel build @rules_oci_runtime//lib/...

if "${run_container}"; then
    bazel run //:container -- /bin/sh -c "echo prebuilt-launcher-ok" | tee output
    grep --quiet --fixed-strings prebuilt-launcher-ok output
fi

echo "Release archive ${version} checks out"
