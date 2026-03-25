#!/usr/bin/env bash

set -euo pipefail

# runc persists container state under `$XDG_RUNTIME_DIR/runc` or `/run/runc` (euid=0,USER=root,not running in user namespace)
# to avoid conflicts (leftover state) we require XDG paths to be used
# this means detecting euid=0 and USER=root
# TODO for completeness also detect if running in user namespace
if [[ $EUID -eq 0 || $USER == "root" ]]; then
  echo "Detected root user, cannot override runc persistent state location, not safe to continue as;"
  echo " - Container IDs may collide"
  echo " - May interfere with other container runtimes (e.g. docker, podman)"
  exit 1
fi

# TODO Because this script is used as a `data` dependency, we need to use templates for these labels
readonly image_loader=$(rlocation "{{image_loader_rlocation}}")
readonly runc_path=$(rlocation "{{runc_rlocation}}")
readonly undocker_path=$(rlocation "{{undocker_rlocation}}")
readonly jq_path=$(rlocation "{{jq_rlocation}}")

# create temporary working directory for image tarball, container and runc state
runc_instance_dir=$(mktemp -d)
echo "Using $runc_instance_dir for runc instance"
image_tar_path="$runc_instance_dir/image.tar"
ctr_dir="$runc_instance_dir/ctr"
mkdir "$ctr_dir"
xdg_runtime_dir_override="$runc_instance_dir/xdg"
mkdir "$xdg_runtime_dir_override"

echo "Writing Docker/OCI image tarball to $image_tar_path..."
OUTPUT="$image_tar_path" $image_loader

# Create base configuration with runc
echo "Writing configuration to $ctr_dir/config.json..."
$runc_path spec --rootless --bundle "$ctr_dir"

# Adjust image configuration
echo "Adjusting container configuration..."
manifest_json=$(tar -x --to-stdout -f "$image_tar_path" manifest.json)
image_config_path=$(echo "$manifest_json" | $jq_path -r '.[0].Config')
image_config_json=$(tar -x --to-stdout -f "$image_tar_path" "$image_config_path")
# IMPORTANT container config must be read in a separate call to avoid races that may produce an empty file
ctr_config=$(cat "$ctr_dir/config.json")
args_as_json=$($jq_path -c -n '$ARGS.positional' --args -- "$@")
echo "$ctr_config" | $jq_path --argjson u "$image_config_json" --argjson a "$args_as_json" '
    .root.readonly = false |
    .process.env = $u.config.Env |
    .process.args = $u.config.Entrypoint + ($u.config.Cmd // []) + $a |
    .process.cwd = $u.config.WorkingDir
' > "$ctr_dir/config.json"

# Create rootfs
echo "Creating rootfs..."
mkdir "${ctr_dir}/rootfs"
echo "Extracting Docker/OCI image tarball to ${ctr_dir}/rootfs..."
$undocker_path $image_tar_path - | tar -C "${ctr_dir}/rootfs" -xf -

echo "Adding host DNS resolver configuration to container..."
if [ -f /etc/resolv.conf ]; then
  mkdir -p "$ctr_dir/rootfs/etc"
  cp /etc/resolv.conf "$ctr_dir/rootfs/etc/resolv.conf"
else
  echo "Warning: Host /etc/resolv.conf not found, skipping copy to container"
fi

echo "Cleaning up Docker/OCI image tarball..."
rm "$image_tar_path"

cleanup() {
  echo "Container has exited, cleaning up..."
  rm -rf "$runc_instance_dir"
}

trap cleanup EXIT

echo "Running container..."
XDG_RUNTIME_DIR="$xdg_runtime_dir_override" $runc_path run --bundle "$ctr_dir" stub_container_id
