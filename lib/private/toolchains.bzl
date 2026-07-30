"""Toolchains for the launcher and the OCI container runtime."""

visibility(["//lib/...", "//docs/..."])

LAUNCHER_TOOLCHAIN_TYPE = Label("//lib:launcher_toolchain_type")

CONTAINER_RUNTIME_TOOLCHAIN_TYPE = Label("//lib:container_runtime_toolchain_type")

_LAUNCHER_DOC = """Declares the launcher that `runc_binary` targets execute.

The launcher reads an OCI image layout, extracts it into a bundle and hands
that bundle to the container runtime. It is not shipped with these rules, so
that building it from source does not force `rules_rust` on every consumer:

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")
```

Register a `launcher_toolchain` to use a patched or prebuilt launcher instead.
"""

_CONTAINER_RUNTIME_DOC = """Declares the OCI runtime that executes a bundle, such as `runc`.

A toolchain for a pinned `runc` release is registered by default. Register your
own earlier in `MODULE.bazel` to take over the version:

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
"""

def _binary_attr(doc):
    # type: (str) -> Attribute
    return attr.label(
        doc = doc,
        mandatory = True,
        allow_files = True,
        executable = True,
        # Both binaries are staged in the runfiles of the target being run, so
        # they are built for the platform the container will run on.
        cfg = "target",
    )

def _toolchain_impl(ctx):
    # type: (ctx) -> list
    return [
        platform_common.ToolchainInfo(
            binary = ctx.executable.binary,
            runfiles = ctx.attr.binary[DefaultInfo].default_runfiles,
        ),
    ]

launcher_toolchain = rule(
    implementation = _toolchain_impl,
    doc = _LAUNCHER_DOC,
    attrs = {"binary": _binary_attr("The launcher executable.")},
)

container_runtime_toolchain = rule(
    implementation = _toolchain_impl,
    doc = _CONTAINER_RUNTIME_DOC,
    attrs = {"binary": _binary_attr("The container runtime executable.")},
)
