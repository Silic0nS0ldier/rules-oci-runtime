# Contributing

## Development

```sh
bazel test //...                          # rule tests and documentation freshness
(cd source && bazel test //...)           # launcher unit tests
(cd e2e/smoke && bazel test //...)        # end to end tests
(cd e2e/conformance && bazel test //...)  # extraction compared against umoci
bazel run //docs:update                   # regenerate docs/defs.md
```

The end to end modules run rootless containers, so they need unprivileged user
namespaces. Distributions that confine those will refuse:

```sh
sudo sysctl --write kernel.apparmor_restrict_unprivileged_userns=0
```

## Releasing

Publishing a release tagged `vX.Y.Z`, notes and all, builds a statically linked launcher for Linux amd64 and arm64, stamps their hashes and the version into a generated ruleset archive, attaches the three to that release along with an installation snippet, then runs a container from them to prove the pins are good. Running the workflow by hand does everything except attach.

Every other CI run builds the same archive with the launchers baked in rather than pinned, since there is no release for them to be pinned to, and uploads it as a workflow artifact versioned `0.0.0-ci`. Such an archive stands alone, so trying a change out before it is released takes nothing but a download:

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime", version = "0.0.0-ci")
archive_override(
    module_name = "rules_oci_runtime",
    strip_prefix = "rules_oci_runtime-0.0.0-ci",
    urls = ["file:///tmp/rules_oci_runtime-0.0.0-ci.tar.gz"],
)
```

That is what the check every archive goes through sets up:

```sh
.github/workflows/check_release_archive.sh \
    --version 0.0.0-ci \
    --archive rules_oci_runtime-0.0.0-ci.tar.gz \
    --run-container
```
