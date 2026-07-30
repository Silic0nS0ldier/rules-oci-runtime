# `rules_oci_runtime`

**WIP**: Bazel rules for running OCI container images (currently using [`runc`](https://github.com/opencontainers/runc)).

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime", version = "0.0.0")

# Builds the launcher from source. Without it, no launcher toolchain exists.
bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")
```

```starlark
# BUILD.bazel
load("@rules_oci_runtime//lib:defs.bzl", "runc_binary")

runc_binary(
    name = "container",
    image = "@alpine",
)
```

```sh
bazel --quiet run //:container -- /bin/sh -c 'echo "Hello, world!"'
```
```
Target //:container up-to-date:
  .bazel/bin/container
Hello, world!
```

As with Docker/OCI runtimes, arguments passed after `--` replace the image `Cmd`.

Set `RULES_OCI_RUNTIME_VERBOSE=1` to log container setup to stderr.

```sh
RULES_OCI_RUNTIME_VERBOSE=1 bazel --quiet run //:container -- /bin/sh -c 'echo "Hello, world!"'
```
```
Target //:container up-to-date:
  .bazel/bin/container
Reading image .../alpine_linux_amd64/layout for linux/amd64
Using /tmp/rules-oci-runtime-c575126f1cd80bad for the container bundle
Extracting layer sha256:2d35ebdb57d9... (application/vnd.oci.image.layer.v1.tar+gzip)
Copying host /etc/resolv.conf into the container
Wrote /tmp/rules-oci-runtime-c575126f1cd80bad/config.json
Handing bundle /tmp/rules-oci-runtime-c575126f1cd80bad to runc
Running container rules-oci-runtime-c575126f1cd80bad
Hello, world!
Container has exited, cleaning up...
```

## Why?

- No dependencies on Docker/Podman.
- Easy to use in devcontainers, only requiring `"privileged": true,`.

## How it works

The image layout directory produced by `rules_oci` is consumed directly, so
there is no image tarball round-trip and no container runtime daemon.

A single Rust binary, the launcher, does all of the work:

1. Reads `index.json`, walks nested indexes and selects the manifest matching
   the requested platform (defaults to the host).
2. Verifies every blob against its digest and size while streaming it.
3. Extracts the layers into a private bundle directory, applying `.wh.`
   whiteouts and rejecting entries that would escape the rootfs.
4. Generates an OCI runtime `config.json` (rootless when run as an
   unprivileged user, otherwise a plain privileged spec).
5. Copies the host `/etc/resolv.conf` and writes `/etc/hosts` and
   `/etc/hostname` so DNS works out of the box.
6. Executes `runc` against a private state root, forwards signals to the
   container, propagates its exit code and removes the bundle afterwards.

Containers share the host network namespace, and each run gets a unique
container ID, so concurrent runs of the same target do not interfere.

## Rules

See [docs/defs.md](docs/defs.md) for the generated API reference.

```starlark
runc_binary(
    name = "container",
    image = "@alpine",
    env = {"GREETING": "hello"},
    hostname = "my-host",
    mounts = ["$BUILD_WORKSPACE_DIRECTORY:/src:ro"],
    read_only = True,
    workdir = "/src",
)
```

`mounts` entries are `SOURCE:DESTINATION[:OPTIONS]`, where `OPTIONS` is a comma
separated mount option list such as `ro` or `rw,noexec`. `$VAR` and `${VAR}` in
`SOURCE` are expanded when the container starts.

## Toolchains

Both binaries a `runc_binary` needs are resolved through toolchains, so either
can be replaced without forking these rules.

| Toolchain type | Binary | Default |
| -------------- | ------ | ------- |
| `//lib:launcher_toolchain_type` | The launcher described above. | None, add `rules_oci_runtime_source`. |
| `//lib:container_runtime_toolchain_type` | An OCI runtime such as `runc`. | A pinned `runc` release. |

The launcher lives in the separate `rules_oci_runtime_source` module so that
building it from source, and therefore depending on `rules_rust`, stays opt in.

To use a patched launcher or a different `runc`, declare a toolchain and
register it before the defaults:

```starlark
# BUILD.bazel
load("@rules_oci_runtime//lib:defs.bzl", "container_runtime_toolchain")

container_runtime_toolchain(
    name = "runc",
    binary = "@my_runc//file",
)

toolchain(
    name = "runc_toolchain",
    target_compatible_with = ["@platforms//os:linux"],
    toolchain = ":runc",
    toolchain_type = "@rules_oci_runtime//lib:container_runtime_toolchain_type",
)
```

```starlark
# MODULE.bazel
register_toolchains("//:runc_toolchain")
```

## Runtime flags

Flags accepted before the container command override the rule attributes:

| Flag | Description |
| ---- | ----------- |
| `-e`, `--env KEY=VALUE` | Add an environment variable. |
| `-v`, `--mount SRC:DST[:OPTS]` | Add a bind mount. |
| `--workdir DIR` | Working directory inside the container. |
| `--hostname NAME` | Container hostname. |
| `--platform OS/ARCH[/VARIANT]` | Select a manifest from a multi-platform image. |
| `--tty auto\|true\|false` | Allocate a terminal. Defaults to `auto`. |
| `--rootless auto\|true\|false` | Use a user namespace. Defaults to `auto`. |
| `--read-only` | Mount the container root filesystem read-only. |
| `--keep-bundle` | Leave the generated bundle on disk for inspection. |
| `--verbose` | Same as `RULES_OCI_RUNTIME_VERBOSE=1`. |

```sh
bazel run //:container -- --env FOO=bar --workdir /tmp /bin/sh -c 'echo "$FOO"'
```

## Development

```sh
bazel test //...                    # rule tests and documentation freshness
(cd source && bazel test //...)     # launcher unit tests
(cd e2e/smoke && bazel test //...)  # end to end tests
bazel run //docs:update             # regenerate docs/defs.md
```
