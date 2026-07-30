#!/usr/bin/env bash

set -euo pipefail

runfiles="${RUNFILES_DIR:-${TEST_SRCDIR:-$0.runfiles}}"
if [[ ! -d "$runfiles" ]]; then
  echo "$0: cannot locate runfiles (looked in ${runfiles})" >&2
  exit 1
fi

# Rule supplied arguments come first so that user arguments can override them.
args=({{args}})

exec "${runfiles}/{{oci_runtime}}" run \
  --layout "${runfiles}/{{layout}}" \
  --runtime "${runfiles}/{{runtime}}" \
  "${args[@]}" \
  "$@"
