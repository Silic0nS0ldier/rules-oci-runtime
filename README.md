# `rules_oci_runtime`

> [!CAUTION]
> This repository is in a proof-of-concept, vibe-coded state.
> It has not been security hardened and is not production ready.
> Time permitting, the plan is to conduct a full rewrite and thorough audit once (and if) value is demonstrated.

Bazel rules for running OCI images defined by [`@rules_oci`](https://github.com/bazel-contrib/rules_oci) _without_ relying on a heavy weight orchestrator like Docker or Podman, instead running an [OCI compliant runtime](https://github.com/opencontainers/runtime-spec) (e.g. [`runc`](https://github.com/opencontainers/runc)) directly.

The ruleset works but is not final, so releases are currently only published to GitHub (not BCR).

## Usage

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime", version = "0.0.0")
archive_override(
    module_name = "rules_oci_runtime",
    integrity = "sha256-...",
    strip_prefix = "rules_oci_runtime-0.0.0",
    urls = ["https://github.com/Silic0nS0ldier/rules-oci-runtime/releases/download/v0.0.0/rules_oci_runtime-0.0.0.tar.gz"],
)
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

## Why?

- No dependencies on Docker/Podman.
- Easy to use in devcontainers, only requiring `"privileged": true,`.

## How it works

The image layout directory produced by `rules_oci` is consumed directly, so
there is no image tarball round-trip and no container runtime daemon.

A single Rust binary, the launcher, does all of the work:

1. Reads `index.json`, walks nested indexes and selects the manifest matching the requested platform (defaults to the host).
2. Verifies every blob against its digest and size while streaming it.
3. Extracts the layers into a private bundle directory, applying `.wh.` whiteouts and rejecting entries that would escape the rootfs.
4. Generates an OCI runtime `config.json` (rootless when run as an unprivileged user, otherwise a plain privileged spec).
5. Copies the host `/etc/resolv.conf` and writes `/etc/hosts` and `/etc/hostname` so DNS works out of the box.
6. Executes `runc` against a private state root, forwards signals to the container, propagates its exit code and removes the bundle afterwards.

Containers share the host network namespace, and each run gets a unique container ID, so concurrent runs of the same target do not interfere.

### Extended attributes

Extended attributes are not restored. The one images use in practice,
`security.capability`, needs a privilege the extraction does not have when it
runs rootless, which is the usual case, and no rootless extractor can restore
it. A container built from such an image therefore does not match it: a binary
that expected a capability will not have one.

Rather than leave that to be found at runtime, an image whose layers set
extended attributes is refused. Pass `--strict-xattrs=false`, or set
`strict_xattrs = False` on the rule, to extract it anyway and drop them; under
`--verbose` each one is named as it is dropped.

They are rare. Across twelve popular images and roughly 152,000 entries, six
entries carried one, all of them `security.capability`.

## Debugging

2 debugging oriented flags exist within the launcher;
1. `--verbose` (or `RULES_OCI_RUNTIME_VERBOSE=1` env var) to log container startup operations.
2. `--keep-bundle` to preserve the [filesystem bundle](https://github.com/opencontainers/runtime-spec/blob/6999a89a76a0329f440d5740497bedb9dd431297/bundle.md) on exit.

e.g.

```sh
bazel --quiet run //:container -- --verbose /bin/sh -c 'echo "Hello, world!"'
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

`mounts` entries are `SOURCE:DESTINATION[:OPTIONS]`, where `OPTIONS` is a comma separated mount option list such as `ro` or `rw,noexec`. `$VAR` and `${VAR}` in `SOURCE` are expanded when the container starts.

## Toolchains

Both binaries a `runc_binary` needs are resolved through toolchains, so either can be replaced without forking these rules.

| Toolchain type | Binary | Default |
| -------------- | ------ | ------- |
| `//lib:launcher_toolchain_type` | The launcher described above. | A prebuilt launcher release. |
| `//lib:container_runtime_toolchain_type` | An OCI runtime such as `runc`. | A pinned `runc` release. |

The launcher is published as a statically linked binary for Linux amd64 and arm64 with each release, and the archive above pins the pair. A source checkout has no release to draw on, so it must build the launcher from source, which pulls in `rules_rust`: add the `rules_oci_runtime_source` module and stand the prebuilt toolchains down.

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")
```

```
# .bazelrc
build --@rules_oci_runtime//lib:prebuilt_launcher=false
```

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
| `--strict-xattrs BOOL` | Refuse an image that sets extended attributes. Defaults to `true`. |
| `--keep-bundle` | Leave the generated bundle on disk for inspection. |
| `--verbose` | Same as `RULES_OCI_RUNTIME_VERBOSE=1`. |

```sh
bazel run //:container -- --env FOO=bar --workdir /tmp /bin/sh -c 'echo "$FOO"'
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test and release the ruleset.
