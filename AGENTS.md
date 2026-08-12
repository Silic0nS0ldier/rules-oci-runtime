# Agent Guidance

This is supplemental to [README.md](README.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

This repository houses the `@rules_oci_runtime` Bazel module. It exists to allow running OCI images defined with [`@rules_oci`](https://github.com/bazel-contrib/rules_oci) _without_ relying on a heavy weight orchestrator like Docker or Podman, instead running an [OCI compliant runtime](https://github.com/opencontainers/runtime-spec) (e.g. [`runc`](https://github.com/opencontainers/runc)) directly.

## Layout

```mermaid
treeView-beta
./ :::highlight ## @rules_oci_runtime, core ruleset.
├── .bazel/
|   ├── bin/ ## Build outputs.
|   ├── out/ ## Shortcut to intermediary build outputs.
|   └── testlogs/ ## Outputs from running tests.
|       ├── **/test.outputs/ ## Any additional outputs produced by tests.
|       ├── **/test.log
|       └── **/test.xml
├── .devcontainer/
|   ├── feature-bazel/ ## Adds Bazel-specific tools to dev environment.
|   └── devcontainer.json
├── .github/
|   ├── workflows/
|   └── renovate.json
├── docs/
├── e2e/*/ ## End-to-end test modules.
├── launcher/BUILD.bazel ## Houses self-contained launcher binaries (e.g. for CI tarball).
├── lib/
|   ├── private/ ## Ruleset implementation details
|   └── defs.bzl ## Public API exports.
├── source/ :::highlight ## @rules_oci_runtime_source, opt-in to build launcher from source.
|   ├── .bazel/
|   ├── src/ ## Launcher source (Rust).
|   └── MODULE.bazel
└── MODULE.bazel
```

Note that testing (`bazel test //...`) must be run within _each_ Bazel module.

## Formatting

The launcher crate (`source/`) is `cargo fmt` clean and `//:oci_runtime_fmt_test`
enforces it in CI. Run `cargo fmt` there before committing, and keep formatting
churn out of feature commits — if a change is formatting only, commit it apart
from the behavioural change.

## Benchmarking

Launcher performance is the recurring workstream, so use the committed tools
rather than writing another one-off script. See
[source/README.md](source/README.md#benchmarking); in short:

```
cd source && bazel build //:bench_image //:bench_run
.bazel/bin/bench_image --output /tmp/bench-full --profile full
.bazel/bin/bench_run --layout /tmp/bench-full --rounds 7 --syscalls OLD NEW
```

- Build with `--config=release`; run outside Bazel.
- Prefer counts to times. Syscall counts and entries placed do not care what
  else the host was doing; wall clock in a container or VM does.
- Anything under 2% is this host, not the change. Prove it by passing the same
  binary to `bench_run` twice before believing a number near the floor.
- Absolute numbers do not survive a reboot. Only within-run comparisons mean
  anything.
- Confirm the fast run did the work: `bench_run` counts the entries every run
  placed and says so when they disagree.
- A harness that skips the work a design introduces will flatter that design.
  The parallel writer looked 5x better than it was for exactly this reason.
- `//:bench_counts_test` guards the counts in CI. When it fails, something
  changed what the launcher asks of the kernel -- find out what before
  updating its numbers.

## Git

- Commit freely on new branches; **never push**. The user pushes and opens PRs.
- Land invasive work as separate or stacked branches rather than one large branch.
- Commit bodies carry the measured numbers behind a performance claim.
- `Refs #N` is for issues only. PR numbers are added by GitHub at merge time.
