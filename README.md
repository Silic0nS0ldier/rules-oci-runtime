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
bazel --quiet run //:container -- -c 'echo "Hello, world!" ; exit'
```
```
Target //:container up-to-date:
  .bazel/bin/container_template
  .bazel/bin/container
Using /tmp/tmp.LRlx5DvoYw for runc instance
Writing Docker/OCI image tarball to /tmp/tmp.LRlx5DvoYw/image.tar...
Writing configuration to /tmp/tmp.LRlx5DvoYw/ctr/config.json...
Creating rootfs...
Extracting docker compliant tarball...
Cleaning up Docker/OCI image tarball...
Running container...
Hello, world!
Container has exited, cleaning up...
```

## Why?

- No dependencies on Docker/Podman.
- Easy to use in devcontainers, only requiring `"privileged": true,`.
