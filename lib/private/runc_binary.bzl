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

def _shell_quote(value):
    # type: (str) -> str
    return "'" + value.replace("'", "'\\''") + "'"

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

    launcher = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.expand_template(
        template = ctx.file._template,
        output = launcher,
        is_executable = True,
        substitutions = {
            "{{args}}": " ".join([_shell_quote(arg) for arg in args]),
            "{{layout}}": _rlocation_path(ctx, layout),
            "{{oci_runtime}}": _rlocation_path(ctx, ctx.executable._oci_runtime),
            "{{runtime}}": _rlocation_path(ctx, ctx.executable._runc),
        },
    )

    runfiles = ctx.runfiles(files = [layout]).merge_all([
        ctx.attr.image[DefaultInfo].default_runfiles,
        ctx.attr._oci_runtime[DefaultInfo].default_runfiles,
        ctx.attr._runc[DefaultInfo].default_runfiles,
    ] + [target[DefaultInfo].default_runfiles for target in ctx.attr.data])

    return [
        DefaultInfo(
            executable = launcher,
            files = depset([launcher]),
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
        "_template": attr.label(
            default = "//lib/private:launcher.tmpl.sh",
            allow_single_file = True,
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
