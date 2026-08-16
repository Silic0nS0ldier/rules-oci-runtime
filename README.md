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
2. Verifies every blob against its digest and size.
3. Puts the layers where the container can see them, applying `.wh.` whiteouts and rejecting entries that would escape the rootfs: [served](#serving-the-rootfs) where the host allows it, extracted where it does not.
4. Generates an OCI runtime `config.json` (rootless when run as an unprivileged user, otherwise a plain privileged spec).
5. Copies the host `/etc/resolv.conf` and writes `/etc/hosts` and `/etc/hostname` so DNS works out of the box.
6. Executes `runc` against a private state root, forwards signals to the container, propagates its exit code and removes the bundle afterwards.

Containers share the host network namespace, and each run gets a unique container ID, so concurrent runs of the same target do not interfere.

### Serving the rootfs

Extraction writes every file an image holds before the container starts, and
most images are mostly files nothing ever opens. When `runc_binary` is built
with `index = True` the layers come with sidecars saying what each one holds and
where inflating can resume, which is enough to describe the whole rootfs without
reading a single byte of it. The launcher mounts that description over FUSE and
fetches a file's bytes the first time something opens it.

A container that reads a handful of files out of
`ghcr.io/browserless/chromium` (22 layers, 2545 MiB of files) starts in 0.7s
this way against 3.0s extracted.

What the container sees is the same tree either way, down to modes, timestamps
and hard link groups, and writes go to the container's own copy rather than back
into the image. Where the host has no `/dev/fuse`, or the image has no sidecars,
the launcher extracts as it always has; `--rootfs` overrides the choice either
way.

Two things are asked of the host and done without where it will not. A mount
that takes itself down when the launcher is killed needs `fusermount3` and,
for an unprivileged caller, `user_allow_other` in `/etc/fuse.conf`; without
it a launcher that is killed outright leaves a mount to be removed by hand.
Handing a file to the kernel to read and write itself, rather than through the
launcher, needs a 6.9 kernel built with `CONFIG_FUSE_PASSTHROUGH` and the
privilege to ask. `--verbose` says which of them the run got.

### Fetching ahead

A served file is paid for when the container opens it, one at a time and with
the container waiting for each. What a container reads barely changes from run
to run, so a recording of one run says what the next will want, and those files
can be fetched before it asks and several at once.

What a container reads is not something a build can work out, so a profile is
recorded by running the container rather than by building it:

```starlark
load("@rules_oci_runtime//lib:defs.bzl", "oci_runtime_profile", "runc_binary")

oci_runtime_profile(
    name = "container_profile",
    srcs = glob(["profiles/container.*.profile"], allow_empty = True),
    record_to = "profiles/container",
)

runc_binary(
    name = "container",
    image = "@alpine",
    profile = ":container_profile",
)
```

```sh
bazel run //:container -- --record-profile /bin/sh -c 'echo "Hello, world!"'
```
```
recorded 4 files this run read into .../profiles/container.linux-amd64.profile, 4 in all over 1 runs
```

The profile is a sorted text file, one path a line with the number of runs that
read it, and recording merges into it rather than replacing it: a container
takes a different path through itself every time it is asked something
different, and recording a few of them covers more than any one of them does.
The name carries the platform because the run only saw one manifest of the
image, and another platform's holds other files.

The container starts as soon as the first file in the profile is there and the
rest are fetched while it runs, giving way to whatever the container asks for
itself; a profile that has gone stale therefore costs a lookup rather than a
wait. `runc_binary` checks each profile against the image it is used with, so
one left behind by a rename, or recorded against another image, fails the build
instead.

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
2. `--keep-bundle` to preserve the [filesystem bundle](https://github.com/opencontainers/runtime-spec/blob/6999a89a76a0329f440d5740497bedb9dd431297/bundle.md) on exit. A served rootfs only exists while the container runs, so what is kept is the files it read; pass `--rootfs=extract` to keep the whole tree.

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
| `--rootfs auto\|fuse\|extract` | Serve the image or extract it. Defaults to `auto`, which serves it where the host and the image allow. |
| `--profile PATH` | Fetch what a recorded profile names ahead of the container. Repeatable. |
| `--record-profile[=PATH]` | Record what this run reads. Without a path it writes where the rule said to, and it serves the image or fails. |
| `--prefetch-barrier COUNT` | How many of a profile's files to fetch before the container starts. Defaults to `1`. |
| `--strict-xattrs BOOL` | Refuse an image that sets extended attributes. Defaults to `true`. |
| `--keep-bundle` | Leave the generated bundle on disk for inspection. |
| `--verbose` | Same as `RULES_OCI_RUNTIME_VERBOSE=1`. |

```sh
bazel run //:container -- --env FOO=bar --workdir /tmp /bin/sh -c 'echo "$FOO"'
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, test and release the ruleset.
