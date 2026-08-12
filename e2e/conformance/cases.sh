#!/usr/bin/env bash
#
# Compares the launcher's extraction against umoci's on the same image.
#
# The unit tests hold the extraction routes to each other and to the spec as
# read here. This holds them to an implementation that was read by someone
# else, which is the only way to catch a clause we have misread the same way
# twice.

set -uo pipefail

runfiles="${RUNFILES_DIR:-${TEST_SRCDIR:-$0.runfiles}}"
launcher="${runfiles}/${LAUNCHER}"
umoci="${runfiles}/${UMOCI}"
work="${TEST_TMPDIR:-/tmp}/conformance"

failures=0

fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# The launcher writes these into the bundle itself, so the reference tree will
# never have them. `etc` is in every fixture for the same reason: the launcher
# would create it for these, and a directory only one side has is a difference
# the fixture caused rather than the extractor.
network_files() {
  grep -vE '^(f|l)\|[0-7]+\|[0-9]+\|etc/(hostname|hosts|resolv\.conf)\|'
}

# A directory's size is how big its inode grew, which says more about the
# filesystem than about the extraction, so it is not compared.
snapshot() {
  (cd "$1" && find . -mindepth 1 -printf '%y|%m|%s|%P|%l\n') |
    awk -F'|' 'BEGIN { OFS = "|" } $1 != "f" { $3 = 0 } { print }' |
    network_files | sort
}

contents() {
  (cd "$1" && find . -type f -exec sha256sum {} + 2>/dev/null) |
    sed 's|  \./|  |' | grep -vE ' etc/(hostname|hosts|resolv\.conf)$' | sort -k2
}

# Two names for one inode have to stay two names for one inode.
link_groups() {
  (cd "$1" && find . -type f -links +1 -printf '%i|%P\n') |
    sort | awk -F'|' '{ print $2 }'
}

compare() {
  local what=$1 ours=$2 reference=$3
  local diffs

  # Two empty trees compare equal, so a fixture that never reached either
  # extractor would pass without having tested anything.
  local entries
  entries=$(snapshot "$reference" | wc -l)
  if ((entries < 3)); then
    fail "${what}: the reference tree has ${entries} entries, so nothing was compared"
    return
  fi

  for probe in snapshot contents link_groups; do
    if ! diffs=$(diff <("$probe" "$ours") <("$probe" "$reference")); then
      fail "${what}: ${probe} differs from umoci (< ours, > umoci)"
      echo "$diffs" | head -20 >&2
    fi
  done
}

# Builds a layer tar from a directory, naming every entry so that the order in
# the archive is the order given rather than whatever the filesystem returns.
pack() {
  local from=$1 out=$2
  shift 2
  (cd "$from" && tar --format=gnu --no-recursion -cf "$out" "$@")
}

# Layers are named `[<compression>:]<path>`. umoci compresses with gzip unless
# told otherwise, which is what every fixture without a prefix gets.
build_image() {
  local layout=$1
  shift
  "$umoci" init --layout "$layout" >/dev/null || return 1
  "$umoci" new --image "${layout}:test" >/dev/null || return 1
  local spec compress layer
  for spec in "$@"; do
    compress=${spec%%:*}
    layer=${spec#*:}
    if [[ "$layer" == "$spec" ]]; then
      compress=gzip
    fi
    "$umoci" raw add-layer --compress="$compress" --image "${layout}:test" "$layer" \
      >/dev/null 2>&1 || return 1
  done
}

# Extracts with the launcher and echoes the rootfs it produced. `/bin/true`
# stands in for the runtime: the bundle is the point, not running it.
extract() {
  local layout=$1 into=$2 index=${3:-}
  local args=(run --layout "$layout" --runtime /bin/true --keep-bundle)
  [[ -n "$index" ]] && args+=(--index "$index")
  mkdir -p "$into"
  TMPDIR="$into" "$launcher" "${args[@]}" /bin/sh >/dev/null 2>&1
  find "$into" -maxdepth 2 -name rootfs
}

# Runs one fixture through umoci and through both of the launcher's routes.
check_fixture() {
  local name=$1
  local layout="${work}/${name}/image"
  shift
  build_image "$layout" "$@" || {
    fail "${name}: could not build the image"
    return
  }

  "$umoci" unpack --rootless --image "${layout}:test" "${work}/${name}/reference" >/dev/null 2>&1
  local reference="${work}/${name}/reference/rootfs"
  [[ -d "$reference" ]] || {
    fail "${name}: umoci produced no rootfs"
    return
  }

  local streamed
  streamed=$(extract "$layout" "${work}/${name}/streamed")
  [[ -n "$streamed" ]] || {
    fail "${name}: the launcher produced no rootfs"
    return
  }
  compare "${name}, streaming" "$streamed" "$reference"

  # With sidecars beside the blobs the launcher resolves the image up front and
  # extracts it span by span, which is a different code path to the walk above.
  # Both compressed formats are checkpointed, so every layer is indexed.
  local index="${work}/${name}/index"
  mkdir -p "$index"
  "$launcher" index --layout "$layout" --output "$index" >/dev/null 2>&1
  local sidecars
  sidecars=$(find "$index" -name '*.zinfo' | wc -l)
  if ((sidecars != $#)); then
    fail "${name}: ${sidecars} checkpoint indexes for $# layer(s)"
  fi
  local indexed
  indexed=$(extract "$layout" "${work}/${name}/indexed" "$index")
  [[ -n "$indexed" ]] || {
    fail "${name}: the launcher produced no rootfs from the index"
    return
  }
  compare "${name}, indexed" "$indexed" "$reference"
}

# `tar -C dir .` writes the archive root into the layer, and the spec's worked
# example lists it first.
case_dot_rooted_layers() {
  local src="${work}/dot/src"
  mkdir -p "$src/etc" "$src/usr/bin"
  printf 'config\n' >"$src/etc/config"
  printf 'binary\n' >"$src/usr/bin/tool"
  ln -s ../usr/bin/tool "$src/etc/tool"
  pack "$src" "${work}/dot/l1.tar" ./ ./etc ./etc/config ./etc/tool ./usr ./usr/bin ./usr/bin/tool
  check_fixture dot "${work}/dot/l1.tar"
}

# A named whiteout, an opaque whiteout, and a path replaced by a later layer.
# Packed under `${work}/$1`, so the compression cases below can have their own
# copy of the same two layers.
whiteout_layers() {
  local at=$1
  local one="${work}/${at}/one" two="${work}/${at}/two"
  mkdir -p "$one/etc" "$one/d/sub" "$two/etc" "$two/d"
  printf 'lower\n' >"$one/etc/config"
  printf 'gone\n' >"$one/etc/removed"
  printf 'one\n' >"$one/d/one"
  printf 'two\n' >"$one/d/sub/two"
  pack "$one" "${work}/${at}/l1.tar" ./ ./d ./d/one ./d/sub ./d/sub/two ./etc ./etc/config ./etc/removed

  printf 'upper\n' >"$two/etc/config"
  : >"$two/etc/.wh.removed"
  : >"$two/d/.wh..wh..opq"
  pack "$two" "${work}/${at}/l2.tar" ./ ./d ./d/.wh..wh..opq ./etc ./etc/config ./etc/.wh.removed
}

case_whiteouts() {
  whiteout_layers wh
  check_fixture wh "${work}/wh/l1.tar" "${work}/wh/l2.tar"
}

# zstd is the other compression the image spec defines, and both sides read it:
# the launcher through `ruzstd`, umoci through `klauspost/compress`.
case_zstd_layers() {
  whiteout_layers zstd
  check_fixture zstd "zstd:${work}/zstd/l1.tar" "zstd:${work}/zstd/l2.tar"
}

# Compression is a property of the layer, not of the image, so the two can be
# mixed -- which is what `oci_image` produces when it adds a zstd layer to a
# base whose layers are gzip.
case_mixed_compression() {
  whiteout_layers mixed
  check_fixture mixed "${work}/mixed/l1.tar" "zstd:${work}/mixed/l2.tar"
}

# "Files that are present in the same layer as a whiteout file can only be
# hidden by whiteout files in subsequent layers", and an opaque whiteout is
# applied before its own layer's entries whatever order they come in.
case_same_layer_whiteout() {
  local one="${work}/same/one" two="${work}/same/two"
  mkdir -p "$one/etc" "$one/d" "$two/etc" "$two/d"
  printf 'lower\n' >"$one/d/hidden"
  printf 'lower\n' >"$one/d/replaced"
  : >"$one/etc/keep"
  pack "$one" "${work}/same/l1.tar" ./ ./d ./d/hidden ./d/replaced ./etc ./etc/keep

  # `kept` is written before the marker that would otherwise hide it.
  printf 'kept\n' >"$two/d/kept"
  : >"$two/d/.wh..wh..opq"
  printf 'upper\n' >"$two/d/replaced"
  : >"$two/d/.wh.replaced"
  pack "$two" "${work}/same/l2.tar" ./ ./d ./d/kept ./d/.wh..wh..opq ./d/replaced ./d/.wh.replaced ./etc
  check_fixture same "${work}/same/l1.tar" "${work}/same/l2.tar"
}

# Hard links, symlinks, and the mode bits above the permission bits.
case_links_and_modes() {
  local src="${work}/links/src"
  mkdir -p "$src/etc" "$src/bin"
  printf 'shared\n' >"$src/bin/first"
  ln "$src/bin/first" "$src/bin/second"
  printf 'suid\n' >"$src/bin/suid"
  chmod 4755 "$src/bin/suid"
  printf 'sticky\n' >"$src/bin/sticky"
  chmod 1777 "$src/bin/sticky"
  ln -s first "$src/bin/link"
  ln "$src/bin/link" "$src/bin/link-to-link" 2>/dev/null || ln -s first "$src/bin/link-to-link"
  : >"$src/etc/keep"
  pack "$src" "${work}/links/l1.tar" ./ ./bin ./bin/first ./bin/second ./bin/suid ./bin/sticky ./bin/link ./bin/link-to-link ./etc ./etc/keep
  check_fixture links "${work}/links/l1.tar"
}

case=${1:?usage: cases.sh CASE}
rm -rf "$work"
mkdir -p "$work"
if ! declare -F "case_${case}" >/dev/null; then
  echo "unknown case ${case}" >&2
  exit 2
fi
"case_${case}"

if ((failures > 0)); then
  echo "${failures} failure(s) in ${case}" >&2
  exit 1
fi
echo "ok: ${case}"
