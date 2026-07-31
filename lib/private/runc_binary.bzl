"""Implementation of `runc_binary`."""

load(
    ":toolchains.bzl",
    "CONTAINER_RUNTIME_TOOLCHAIN_TYPE",
    "LAUNCHER_TOOLCHAIN_TYPE",
)

visibility(["//lib/...", "//docs/..."])

_DOC = """Creates an executable that runs an OCI image with `runc`.

The image layout produced by `rules_oci` is consumed directly: no image tarball
is written and no container runtime daemon is required.

```starlark
load("@rules_oci_runtime//lib:defs.bzl", "runc_binary")

runc_binary(
    name = "container",
    image = "@alpine",
)
```

```sh
bazel run //:container -- /bin/sh -c 'echo "Hello, world!"'
```

Arguments after `--` replace the image `Cmd`, as with Docker and other OCI
runtimes. Set `RULES_OCI_RUNTIME_VERBOSE=1` to log container setup to stderr.

A `launcher_toolchain` must be registered, which the `rules_oci_runtime_source`
module does.
"""

_NO_LAUNCHER = """{}: no launcher toolchain is registered.

The prebuilt launcher is disabled by
`--@rules_oci_runtime//lib:prebuilt_launcher=false`. Either drop that flag, or
add the module that builds the launcher from source to MODULE.bazel:

    bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")

or register your own `launcher_toolchain`."""

def _rlocation_path(ctx, file):
    # type: (ctx, File) -> str
    if file.short_path.startswith("../"):
        return file.short_path[3:]
    return ctx.workspace_name + "/" + file.short_path

def _image_layout(ctx):
    # type: (ctx) -> File
    files = ctx.attr.image[DefaultInfo].files.to_list()
    directories = [file for file in files if file.is_directory]
    if len(directories) != 1:
        fail(
            "{}: image {} must provide exactly one OCI image layout directory, got {}".format(
                ctx.label,
                ctx.attr.image.label,
                [file.short_path for file in files],
            ),
        )
    return directories[0]

def _runc_binary_impl(ctx):
    # type: (ctx) -> list
    launcher_toolchain = ctx.toolchains[LAUNCHER_TOOLCHAIN_TYPE]
    if not launcher_toolchain:
        fail(_NO_LAUNCHER.format(ctx.label))
    runtime_toolchain = ctx.toolchains[CONTAINER_RUNTIME_TOOLCHAIN_TYPE]

    layout = _image_layout(ctx)

    args = []
    for name, value in ctx.attr.env.items():
        args.extend(["--env", "{}={}".format(name, value)])
    for mount in ctx.attr.mounts:
        args.extend(["--mount", mount])
    if ctx.attr.workdir:
        args.extend(["--workdir", ctx.attr.workdir])
    if ctx.attr.hostname:
        args.extend(["--hostname", ctx.attr.hostname])
    if ctx.attr.read_only:
        args.append("--read-only")

    # The launcher is its own wrapper: it reads the sidecar config found next to
    # `argv[0]`, so no shell script is needed.
    launcher = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(
        output = launcher,
        target_file = launcher_toolchain.binary,
        is_executable = True,
    )

    config = ctx.actions.declare_file(ctx.label.name + ".launch.json")
    ctx.actions.write(
        output = config,
        content = json.encode({
            "layout": _rlocation_path(ctx, layout),
            "runtime": _rlocation_path(ctx, runtime_toolchain.binary),
            "args": args,
        }),
    )

    runfiles = ctx.runfiles(files = [
        layout,
        config,
        launcher_toolchain.binary,
        runtime_toolchain.binary,
    ]).merge_all([
        launcher_toolchain.runfiles,
        runtime_toolchain.runfiles,
        ctx.attr.image[DefaultInfo].default_runfiles,
    ] + [target[DefaultInfo].default_runfiles for target in ctx.attr.data])

    return [
        DefaultInfo(
            executable = launcher,
            files = depset([launcher, config]),
            runfiles = runfiles,
        ),
    ]

runc_binary = rule(
    implementation = _runc_binary_impl,
    doc = _DOC,
    executable = True,
    attrs = {
        "image": attr.label(
            mandatory = True,
            doc = "An OCI image layout directory, such as an `oci_image` or `oci.pull` target.",
        ),
        "env": attr.string_dict(
            doc = "Environment variables added to the container, overriding the image.",
        ),
        "mounts": attr.string_list(
            doc = """Bind mounts, each `SOURCE:DESTINATION[:OPTIONS]`.

`OPTIONS` is a comma separated mount option list such as `ro` or `rw,noexec`,
defaulting to `rw`. `$VAR` and `${VAR}` are expanded in `SOURCE` when the
container starts, so `$BUILD_WORKSPACE_DIRECTORY:/src:ro` mounts the workspace.
""",
        ),
        "workdir": attr.string(
            doc = "Working directory inside the container, overriding the image `WorkingDir`.",
        ),
        "hostname": attr.string(
            doc = "Hostname inside the container. Defaults to `container`.",
        ),
        "read_only": attr.bool(
            doc = "Mount the container root filesystem read-only.",
        ),
        "data": attr.label_list(
            allow_files = True,
            doc = "Extra files to make available in the runfiles tree, for use with `mounts`.",
        ),
    },
    toolchains = [
        config_common.toolchain_type(LAUNCHER_TOOLCHAIN_TYPE, mandatory = False),
        CONTAINER_RUNTIME_TOOLCHAIN_TYPE,
    ],
)
