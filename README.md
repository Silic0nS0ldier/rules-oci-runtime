# `rules_oci_runtime`

**WIP**: Bazel rules for running OCI container images (currently using [`runc`](https://github.com/opencontainers/runc)).

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
  .bazel/bin/container_template
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
  .bazel/bin/container_template
  .bazel/bin/container
Using /tmp/tmp.LRlx5DvoYw for runc instance
Writing Docker/OCI image tarball to /tmp/tmp.LRlx5DvoYw/image.tar...
Piping image from /dev/fd/63 to /tmp/tmp.LRlx5DvoYw/image.tar
Done.
Writing configuration to /tmp/tmp.LRlx5DvoYw/ctr/config.json...
Adjusting container configuration...
Creating rootfs...
Extracting Docker/OCI image tarball to /tmp/tmp.LRlx5DvoYw/ctr/rootfs...
Adding host DNS resolver configuration to container...
Cleaning up Docker/OCI image tarball...
Running container...
Hello, world!
Container has exited, cleaning up...
```

## Why?

- No dependencies on Docker/Podman.
- Easy to use in devcontainers, only requiring `"privileged": true,`.
