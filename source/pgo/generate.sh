#!/usr/bin/env bash
# Records a profile of the release launcher extracting a generated image, and
# prints the absolute path of the merged `.profdata` for `--//pgo:profile`.
#
# The file is named for its contents: Bazel keys the build on the path alone, so
# a profile rewritten in place would be a cache hit on the old one.
set -o errexit -o nounset -o pipefail

usage() {
    echo "Usage: $0 --output-dir DIR [--image full|medium|small]" >&2
    exit 2
}

image=full
output_dir=""
while (($#)); do
    case "$1" in
        --image) image="$2"; shift 2 ;;
        --output-dir) output_dir="$2"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "${output_dir}" ]] || usage

# Only the path goes to stdout, so a caller can capture it.
exec 3>&1 1>&2

mkdir -p "${output_dir}"
output_dir="$(cd "${output_dir}" && pwd)"
cd "$(dirname "${BASH_SOURCE[0]}")/.."
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# The directory baked in is fixed so the instrumented build stays cacheable;
# `LLVM_PROFILE_FILE` sends the data elsewhere at run time. Copied out, as the
# next build points `.bazel/bin` at another configuration.
bazel build --config=release \
    --@rules_rust//rust/settings:extra_rustc_flag=-Cprofile-generate=/tmp/pgo-data \
    --remote_download_outputs=toplevel \
    //:oci_runtime
cp .bazel/bin/oci_runtime "${work}/oci_runtime"

bazel build --remote_download_outputs=toplevel //:bench_image
.bazel/bin/bench_image --output "${work}/image" --profile "${image}"

export LLVM_PROFILE_FILE="${work}/profraw/%m_%p.profraw"
"${work}/oci_runtime" index --layout "${work}/image" --output "${work}/index"

# Both routes, with sidecars and without. `--rootfs=extract` because, given
# sidecars, a host with FUSE would otherwise serve the image and extract
# nothing. Each gets a store of its own, so neither finds the other's files.
for route in indexed planned; do
    index=()
    if [[ "${route}" == indexed ]]; then
        index=(--index "${work}/index")
    fi
    mkdir "${work}/${route}"
    # The runtime is absent, so the launcher fails once the bundle is ready.
    TMPDIR="${work}/${route}" "${work}/oci_runtime" run \
        --layout "${work}/image" "${index[@]}" \
        --rootfs=extract --cache-dir "${work}/store-${route}" \
        --runtime /nonexistent/runc --keep-bundle || true
    if ! compgen -G "${work}/${route}/*/rootfs/*" >/dev/null; then
        echo "The ${route} route extracted nothing to train on." >&2
        exit 1
    fi
done

bazel run //pgo:llvm_profdata -- \
    merge --output="${work}/merged.profdata" "${work}"/profraw/*.profraw

digest="$(sha256sum "${work}/merged.profdata" | cut -c 1-16)"
profile="${output_dir}/launcher.${digest}.profdata"
mv "${work}/merged.profdata" "${profile}"
echo "${profile}" >&3
