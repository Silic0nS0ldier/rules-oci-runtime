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

Launcher performance is the recurring workstream, so measure it properly:

- Use an optimised release build (`--config=release`).
- Report CPU time (user + sys) and syscall counts. Wall clock is unreliable in
  containers and VMs.
- Interleave old and new binaries round robin; report min, median and max.
- Benchmark on an image rich in symlinks and overwrites, not just a distro base.
  A per entry cache regression that was invisible on alpine cost 5x on
  `browserless/chromium`.
- Confirm the fast run actually did the work (entry counts, exit codes) and diff
  the extracted trees before believing a speedup.

## Git

- Commit freely; **never push**. The user pushes and opens PRs.
- Land invasive work as separate or stacked PRs rather than one large branch.
- Commit bodies carry the measured numbers behind a performance claim.
- `Refs #N` is for issues only. PR numbers are added by GitHub at merge time.
