"""Implementation of `runc_binary`."""

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
"""

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

    # The runtime doubles as its own launcher: it reads the sidecar config found
    # next to `argv[0]`, so no shell wrapper is needed.
    launcher = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(
        output = launcher,
        target_file = ctx.executable._oci_runtime,
        is_executable = True,
    )

    config = ctx.actions.declare_file(ctx.label.name + ".launch.json")
    ctx.actions.write(
        output = config,
        content = json.encode({
            "layout": _rlocation_path(ctx, layout),
            "runtime": _rlocation_path(ctx, ctx.executable._runc),
            "args": args,
        }),
    )

    runfiles = ctx.runfiles(files = [layout, config, ctx.executable._oci_runtime]).merge_all([
        ctx.attr.image[DefaultInfo].default_runfiles,
        ctx.attr._oci_runtime[DefaultInfo].default_runfiles,
        ctx.attr._runc[DefaultInfo].default_runfiles,
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
        "_oci_runtime": attr.label(
            default = "//lib/private/oci_runtime",
            executable = True,
            cfg = "target",
        ),
        "_runc": attr.label(
            default = "@multitool//tools/runc",
            executable = True,
            cfg = "target",
        ),
    },
)
