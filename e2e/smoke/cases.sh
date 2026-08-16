#!/usr/bin/env bash
#
# End to end cases for `runc_binary`. Each case is a separate `sh_test` target
# so failures point at the behaviour that broke.

set -uo pipefail

runfiles="${RUNFILES_DIR:-${TEST_SRCDIR:-$0.runfiles}}"
container="${runfiles}/${CONTAINER}"
configured_container="${runfiles}/${CONFIGURED_CONTAINER}"
read_only_container="${runfiles}/${READ_ONLY_CONTAINER}"
mounting_container="${runfiles}/${MOUNTING_CONTAINER}"
profiled_container="${runfiles}/${PROFILED_CONTAINER}"
zstd_container="${runfiles}/${ZSTD_CONTAINER}"

failures=0

fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

assert_equals() {
  local expected="$1" actual="$2" what="$3"
  if [[ "$expected" != "$actual" ]]; then
    fail "${what}: expected '${expected}', got '${actual}'"
  fi
}

assert_contains() {
  local haystack="$1" needle="$2" what="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    fail "${what}: expected output to contain '${needle}', got '${haystack}'"
  fi
}

assert_not_contains() {
  local haystack="$1" needle="$2" what="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    fail "${what}: expected output not to contain '${needle}', got '${haystack}'"
  fi
}

case_default_cmd() {
  # The alpine image's Cmd is /bin/sh, which reads the script from stdin.
  local output
  output=$(echo 'echo from-image-cmd' | "$container")
  assert_equals "from-image-cmd" "$output" "image Cmd"
}

case_command_override() {
  local output
  output=$("$container" /bin/echo command-override </dev/null)
  assert_equals "command-override" "$output" "command override"
}

case_exit_code() {
  "$container" /bin/sh -c 'exit 42' </dev/null
  assert_equals "42" "$?" "exit code propagation"

  "$container" /bin/true </dev/null
  assert_equals "0" "$?" "successful exit code"
}

case_missing_command() {
  local output status
  output=$("$container" /definitely/not/a/binary </dev/null 2>&1)
  status=$?
  if [[ "$status" -eq 0 ]]; then
    fail "running a missing binary should fail, got status 0 and output '${output}'"
  fi
}

case_image_env() {
  local output
  output=$("$container" /bin/sh -c 'echo "$PATH"' </dev/null)
  assert_equals "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    "$output" "image PATH"
}

case_rule_env_and_workdir() {
  local output
  output=$("$configured_container" /bin/busybox sh -c 'echo "$GREETING"; echo "$PATH"; pwd' </dev/null)
  assert_contains "$output" "hello-from-the-rule" "rule env"
  assert_contains "$output" "/rule/bin" "rule env overriding the image PATH"
  assert_contains "$output" "/etc" "rule workdir"

  # A command line --env must win over the rule attribute.
  output=$("$configured_container" --env GREETING=from-the-command-line \
    /bin/busybox sh -c 'echo "$GREETING"' </dev/null)
  assert_equals "from-the-command-line" "$output" "command line env override"
}

case_runtime_mount() {
  local source="${TEST_TMPDIR}/runtime-mount"
  mkdir -p "$source"
  echo "mounted-at-runtime" >"${source}/file.txt"

  local output
  output=$("$container" --mount "${source}:/data:ro" \
    /bin/sh -c 'cat /data/file.txt; touch /data/new 2>&1 || echo read-only-enforced' </dev/null)
  assert_contains "$output" "mounted-at-runtime" "runtime bind mount"
  assert_contains "$output" "read-only-enforced" "read-only mount option"
}

case_rule_mounts() {
  local source="${TEST_TMPDIR}/rule-mount"
  mkdir -p "$source"
  echo "mounted-by-the-rule" >"${source}/file.txt"

  local output
  output=$("$mounting_container" /bin/cat /rule-data/file.txt </dev/null)
  assert_equals "mounted-by-the-rule" "$output" "rule bind mount with \$TEST_TMPDIR expansion"
}

case_read_only_rootfs() {
  local output
  output=$("$read_only_container" \
    /bin/sh -c 'touch /should-fail 2>&1 || echo rootfs-is-read-only' </dev/null)
  assert_contains "$output" "rootfs-is-read-only" "read-only root filesystem"

  output=$("$container" /bin/sh -c 'touch /should-work && echo rootfs-is-writable' </dev/null)
  assert_contains "$output" "rootfs-is-writable" "writable root filesystem by default"
}

case_dns_configuration() {
  local output
  output=$("$container" /bin/sh -c 'test -s /etc/resolv.conf && echo resolv-conf-present' </dev/null)
  assert_contains "$output" "resolv-conf-present" "host resolver configuration"
}

case_hostname_and_hosts() {
  local output
  output=$("$configured_container" /bin/busybox sh -c '/bin/busybox hostname; /bin/busybox cat /etc/hostname /etc/hosts' </dev/null)
  assert_contains "$output" "e2e-host" "configured hostname"
  assert_contains "$output" "127.0.0.1" "generated /etc/hosts"

  output=$("$container" /bin/sh -c 'hostname' </dev/null)
  assert_equals "container" "$output" "default hostname"
}

case_layer_indexes() {
  local config="${container}.launch.json"
  local index_path
  index_path=$(sed -n 's/.*"index":"\([^"]*\)".*/\1/p' "$config")
  if [[ -z "$index_path" ]]; then
    fail "no index entry in $(cat "$config")"
    return
  fi

  local index_dir="${runfiles}/${index_path}"
  local checkpoints=0 tables=0 entry
  for entry in "$index_dir"/*; do
    case "$(basename "$entry")" in
      [0-9a-f]*.zinfo) checkpoints=$((checkpoints + 1)) ;;
      [0-9a-f]*.entries) tables=$((tables + 1)) ;;
      *) fail "unexpected index entry ${entry}" ;;
    esac
    if [[ ! -s "$entry" ]]; then
      fail "index ${entry} is empty"
    fi
  done
  if [[ "$checkpoints" -eq 0 ]]; then
    fail "no layer indexes in ${index_dir}"
  fi
  if [[ "$tables" -ne "$checkpoints" ]]; then
    fail "expected an entry table per layer index, got ${tables} and ${checkpoints}"
  fi

  # The launcher must actually consume the indexes during extraction. It
  # deliberately ignores them on a single core, where they cannot help.
  if [[ "$(nproc)" -lt 2 ]]; then
    return
  fi
  local stderr
  RULES_OCI_RUNTIME_VERBOSE=1 "$container" --rootfs=extract /bin/true </dev/null 2>"${TEST_TMPDIR}/index.err" ||
    fail "container failed"
  stderr=$(cat "${TEST_TMPDIR}/index.err")
  assert_contains "$stderr" "checkpoints" "layers extract via their indexes"
}

# A zstd layer on a gzip base: compression is a property of the layer, and
# `oci_image` writes whichever the archive it was handed uses.
case_zstd_layer() {
  local output stderr
  output=$(RULES_OCI_RUNTIME_VERBOSE=1 "$zstd_container" --rootfs=extract \
    /bin/sh -c 'cat /zstd-marker; cat /etc/alpine-release' \
    </dev/null 2>"${TEST_TMPDIR}/zstd.err")
  assert_contains "$output" "from-the-zstd-layer" "the file the zstd layer adds"
  assert_contains "$output" "3.22" "the gzip base underneath it"

  # Without this the case would pass on a gzip layer too, which is what
  # `oci_image` writes if the archive is not the one we think it is. The span
  # route does not name each layer as it goes, so the image says so instead.
  local layout
  layout=$(sed -n 's/.*"layout":"\([^"]*\)".*/\1/p' "${zstd_container}.launch.json")
  if ! grep -rq 'tar+zstd' "${runfiles}/${layout}/blobs"; then
    fail "no zstd layer in ${runfiles}/${layout}"
  fi

  # Both formats are checkpointed now, so a zstd layer no longer takes the
  # image off the parallel route.
  stderr=$(cat "${TEST_TMPDIR}/zstd.err")
  if [[ "$(nproc)" -ge 2 ]]; then
    assert_contains "$stderr" "units on" "a zstd layer still takes the span route"
  fi
}

case_rootfs_contents() {
  local output
  output=$("$container" /bin/sh -c 'cat /etc/alpine-release; test -L /bin/sh && echo sh-is-a-symlink' </dev/null)
  assert_contains "$output" "3.22" "extracted image contents"
  assert_contains "$output" "sh-is-a-symlink" "symlinks preserved during extraction"
}

case_stdin_is_piped() {
  local output
  output=$(printf 'line-one\nline-two\n' | "$container" /bin/cat)
  assert_equals $'line-one\nline-two' "$output" "stdin forwarded to the container"
}

case_verbose_logging() {
  local stdout stderr
  stdout=$("$container" /bin/echo only-container-output </dev/null 2>"${TEST_TMPDIR}/quiet.err")
  stderr=$(cat "${TEST_TMPDIR}/quiet.err")
  assert_equals "only-container-output" "$stdout" "quiet stdout"
  assert_equals "" "$stderr" "quiet stderr"

  stdout=$(RULES_OCI_RUNTIME_VERBOSE=1 "$container" --rootfs=extract /bin/echo only-container-output \
    </dev/null 2>"${TEST_TMPDIR}/verbose.err")
  stderr=$(cat "${TEST_TMPDIR}/verbose.err")
  assert_equals "only-container-output" "$stdout" "verbose stdout"
  assert_contains "$stderr" "Extracting" "verbose setup logging"
  assert_not_contains "$stdout" "Extracting" "setup logging kept off stdout"
}

# The image is served rather than extracted wherever the host allows it, so the
# same container has to come out the same either way.
case_served_rootfs() {
  local extracted served stderr
  extracted=$("$container" --rootfs=extract \
    /bin/sh -c 'cat /etc/alpine-release; readlink /bin/sh; ls /etc | wc -l' </dev/null)

  served=$(RULES_OCI_RUNTIME_VERBOSE=1 "$container" --rootfs=fuse \
    /bin/sh -c 'cat /etc/alpine-release; readlink /bin/sh; ls /etc | wc -l' \
    </dev/null 2>"${TEST_TMPDIR}/served.err")
  stderr=$(cat "${TEST_TMPDIR}/served.err")
  # A host without /dev/fuse cannot serve at all, and says so rather than
  # quietly extracting when the route is asked for by name.
  if [[ "$stderr" == *"cannot serve the image"* ]]; then
    return
  fi

  assert_contains "$stderr" "Serving" "the image is served"
  assert_equals "$extracted" "$served" "a served rootfs reads as an extracted one"

  # Writes go to the container's own copy, not back into the image.
  served=$("$container" --rootfs=fuse /bin/sh -c \
    'echo written > /etc/alpine-release; cat /etc/alpine-release' </dev/null)
  assert_equals "written" "$served" "a served rootfs is writable"
  served=$("$container" --rootfs=fuse /bin/cat /etc/alpine-release </dev/null)
  assert_equals "${extracted%%$'\n'*}" "$served" "the next run sees the image again"
}

# What a container read is recorded from a run of it and fetched ahead of the
# next one. Recording is a run time thing rather than a build time one, so this
# records into the test's own directory rather than a source tree.
case_recorded_profile() {
  local base="${TEST_TMPDIR}/profiles/alpine"
  local platform err
  err="${TEST_TMPDIR}/record.err"

  RULES_OCI_RUNTIME_VERBOSE=1 "$container" --record-profile="$base" \
    /bin/sh -c 'cat /etc/alpine-release >/dev/null' </dev/null 2>"$err"
  if grep -q "cannot serve the image" "$err"; then
    return
  fi

  local profile
  profile=$(echo "${base}".*.profile)
  if [[ ! -f "$profile" ]]; then
    fail "recording wrote no profile: $(cat "$err")"
    return
  fi
  # The platform is in the name so two of them cannot land on one file.
  assert_contains "$profile" "$(uname -s | tr 'A-Z' 'a-z')-" "the profile is named for a platform"
  assert_contains "$(cat "$profile")" "/etc/alpine-release" "the file the container read"
  assert_contains "$(cat "$profile")" "runs 1" "one recorded run"

  # A second recording adds to the first rather than replacing it.
  "$container" --record-profile="$base" \
    /bin/sh -c 'cat /etc/hostname >/dev/null' </dev/null 2>/dev/null
  assert_contains "$(cat "$profile")" "runs 2" "a second recorded run"
  assert_contains "$(cat "$profile")" "/etc/alpine-release" "what the first run read"
  assert_equals "1" "$(grep -c ' /etc/alpine-release$' "$profile")" "one line per file"

  # Reading it back fetches those files before the container asks for them.
  local output
  output=$(RULES_OCI_RUNTIME_VERBOSE=1 "$container" --rootfs=fuse --profile "$profile" \
    /bin/cat /etc/alpine-release </dev/null 2>"${TEST_TMPDIR}/replay.err")
  assert_equals "$("$container" --rootfs=extract /bin/cat /etc/alpine-release </dev/null)" \
    "$output" "a profiled run reads what an extracted one does"
  assert_contains "$(cat "${TEST_TMPDIR}/replay.err")" "ahead of the container" "files fetched ahead"
  # The point of the profile: nothing the container opened had to be fetched
  # while it waited.
  assert_contains "$(cat "${TEST_TMPDIR}/replay.err")" "waited for 0 files" "no blocking fetches"

  # There is nothing to record from a rootfs that is written out in full before
  # the container starts.
  "$container" --rootfs=extract --record-profile="$base" /bin/true </dev/null \
    2>"${TEST_TMPDIR}/extracted.err"
  assert_contains "$(cat "${TEST_TMPDIR}/extracted.err")" "nothing to record" \
    "recording from an extracted rootfs"

  # A profile recorded for another platform is not this one's to use.
  local other="${TEST_TMPDIR}/other.linux-nosucharch.profile"
  sed 's|^platform .*|platform linux/nosucharch|' "$profile" >"$other"
  RULES_OCI_RUNTIME_VERBOSE=1 "$container" --rootfs=fuse --profile "$other" \
    /bin/true </dev/null 2>"${TEST_TMPDIR}/other.err"
  assert_contains "$(cat "${TEST_TMPDIR}/other.err")" "Ignoring" "another platform's profile"

  # The rule wires its own profile in, whether or not one has been recorded.
  output=$("$profiled_container" /bin/cat /etc/alpine-release </dev/null)
  assert_equals "$("$container" --rootfs=extract /bin/cat /etc/alpine-release </dev/null)" \
    "$output" "the rule's profiled container"
}

# A bundle goes away with the launcher however the launcher goes. A served
# rootfs is a mount rather than a directory, and one left standing needs a hand
# to remove, so the host is asked for a mount that takes itself down.
case_auto_unmount() {
  local out="${TEST_TMPDIR}/auto-unmount.out" err="${TEST_TMPDIR}/auto-unmount.err"
  RULES_OCI_RUNTIME_VERBOSE=1 "$container" --rootfs=fuse \
    /bin/sh -c 'echo ready; sleep 30' </dev/null >"$out" 2>"$err" &
  local pid=$!

  local waited=0
  while ! grep -q ready "$out" 2>/dev/null; do
    sleep 0.2
    waited=$((waited + 1))
    if [[ "$waited" -gt 150 ]]; then
      kill -9 "$pid" 2>/dev/null
      fail "container never started"
      return
    fi
  done

  local rootfs
  rootfs=$(sed -n 's/^Serving .* at \(.*\) on [0-9]* threads$/\1/p' "$err")
  if [[ -z "$rootfs" ]]; then
    fail "the image was not served: $(cat "$err")"
    kill -9 "$pid" 2>/dev/null
    return
  fi
  if ! grep -q "goes away with this process" "$err"; then
    # No `fusermount3`, or a host that will not let this user open a mount to
    # others. Nothing to hold the launcher to then, but say so: a case that
    # quietly tests nothing is worse than one that fails.
    echo "SKIP: this host does not give out mounts that take themselves down" >&2
    kill -9 "$pid" 2>/dev/null
    return
  fi
  if ! grep -qF " ${rootfs} " /proc/self/mountinfo; then
    fail "nothing is mounted at ${rootfs}"
    kill -9 "$pid" 2>/dev/null
    return
  fi

  kill -9 "$pid"
  waited=0
  while grep -qF " ${rootfs} " /proc/self/mountinfo; do
    sleep 0.2
    waited=$((waited + 1))
    if [[ "$waited" -gt 50 ]]; then
      fail "the mount at ${rootfs} outlived the launcher"
      return
    fi
  done
}

case_signal_forwarding() {
  "$container" /bin/sh -c 'trap "echo caught; exit 7" TERM; echo ready; while true; do sleep 0.1; done' \
    </dev/null >"${TEST_TMPDIR}/signal.out" &
  local pid=$!

  local waited=0
  while ! grep -q ready "${TEST_TMPDIR}/signal.out" 2>/dev/null; do
    sleep 0.2
    waited=$((waited + 1))
    if [[ "$waited" -gt 150 ]]; then
      kill -9 "$pid" 2>/dev/null
      fail "container never started"
      return
    fi
  done

  kill -TERM "$pid"
  wait "$pid"
  assert_equals "7" "$?" "exit code after SIGTERM"
  assert_contains "$(cat "${TEST_TMPDIR}/signal.out")" "caught" "SIGTERM forwarded to the container"
}

case_concurrent_runs() {
  local pids=()
  for i in 1 2 3 4; do
    "$container" /bin/echo "run-${i}-ok" </dev/null >"${TEST_TMPDIR}/concurrent-${i}.out" &
    pids+=($!)
  done
  for pid in "${pids[@]}"; do
    wait "$pid" || fail "a concurrent run failed"
  done
  for i in 1 2 3 4; do
    assert_equals "run-${i}-ok" "$(cat "${TEST_TMPDIR}/concurrent-${i}.out")" "concurrent run ${i}"
  done
}

case_cleanup() {
  RULES_OCI_RUNTIME_VERBOSE=1 "$container" /bin/true </dev/null 2>"${TEST_TMPDIR}/cleanup.err" ||
    fail "container failed"

  local bundle
  bundle=$(sed -n 's/^Using \(.*\) for the container bundle$/\1/p' "${TEST_TMPDIR}/cleanup.err")
  if [[ -z "$bundle" ]]; then
    fail "could not determine the bundle path from: $(cat "${TEST_TMPDIR}/cleanup.err")"
    return
  fi
  if [[ -e "$bundle" ]]; then
    fail "bundle ${bundle} still exists after the container exited"
  fi

  # Removal is handed to a detached process, so the staged copy goes shortly after.
  local waited=0
  while [[ -e "${bundle}.removing" ]]; do
    sleep 0.2
    waited=$((waited + 1))
    if [[ "$waited" -gt 150 ]]; then
      fail "staged bundle ${bundle}.removing was never removed"
      return
    fi
  done
}

main() {
  local case_name="${1:-}"
  if [[ -z "$case_name" ]]; then
    echo "usage: $0 <case>" >&2
    exit 2
  fi
  if ! declare -F "case_${case_name}" >/dev/null; then
    echo "unknown case: ${case_name}" >&2
    exit 2
  fi
  "case_${case_name}"
  if [[ "$failures" -gt 0 ]]; then
    echo "${failures} assertion(s) failed in ${case_name}" >&2
    exit 1
  fi
  echo "ok: ${case_name}"
}

main "$@"
