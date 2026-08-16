"""Implementation of `runc_binary`."""

load(":profile.bzl", "OciRuntimeProfileInfo")
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

Toolchains for a prebuilt launcher and `runc` are registered by default, so
adding the module is all the setup needed on Linux amd64 and arm64.
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
    launcher_toolchain = ctx.toolchains[LAUNCHER_TOOLCHAIN_TYPE]
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
    if not ctx.attr.strict_xattrs:
        args.append("--strict-xattrs=false")

    # The launcher is its own wrapper: it reads the sidecar config found next to
    # `argv[0]`, so no shell script is needed.
    launcher = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(
        output = launcher,
        target_file = launcher_toolchain.binary,
        is_executable = True,
    )

    content = {
        "layout": _rlocation_path(ctx, layout),
        "runtime": _rlocation_path(ctx, runtime_toolchain.binary),
        "args": args,
    }

    indexes = []
    if ctx.attr.index:
        index_dir = ctx.actions.declare_directory(ctx.label.name + ".zinfo")
        ctx.actions.run(
            executable = ctx.attr._tool[DefaultInfo].files_to_run,
            arguments = ["index", "--layout", layout.path, "--output", index_dir.path],
            inputs = [layout],
            outputs = [index_dir],
            mnemonic = "OciLayerIndex",
            progress_message = "Indexing gzip layers of %{label}",
        )
        content["index"] = _rlocation_path(ctx, index_dir)
        indexes.append(index_dir)

    profiles, checks = _profiles(ctx, layout, indexes)
    if profiles:
        content["profiles"] = [_rlocation_path(ctx, profile) for profile in profiles]
    if ctx.attr.profile:
        content["record_to"] = ctx.attr.profile[OciRuntimeProfileInfo].record_to

    config = ctx.actions.declare_file(ctx.label.name + ".launch.json")
    ctx.actions.write(
        output = config,
        content = json.encode(content),
    )

    runfiles = ctx.runfiles(files = [
        layout,
        config,
        launcher_toolchain.binary,
        runtime_toolchain.binary,
    ] + indexes + profiles).merge_all([
        launcher_toolchain.runfiles,
        runtime_toolchain.runfiles,
        ctx.attr.image[DefaultInfo].default_runfiles,
    ] + [target[DefaultInfo].default_runfiles for target in ctx.attr.data])

    return [
        DefaultInfo(
            executable = launcher,
            files = depset([launcher, config] + indexes),
            runfiles = runfiles,
        ),
        OutputGroupInfo(_validation = depset(checks)),
    ]

def _profiles(ctx, layout, indexes):
    # type: (ctx, File, list) -> tuple
    """The profiles to fetch ahead from, and the checks that they still apply.

    A profile that has stopped describing the image costs a lookup and nothing
    else at run time, so nothing fails there. Here is where saying so is any
    use: a profile left behind by a rename, or recorded against another image,
    still looks like a profile and no longer buys what it claims.
    """
    if not ctx.attr.profile:
        return [], []
    profiles = ctx.attr.profile[OciRuntimeProfileInfo].files
    if profiles and not indexes:
        fail("{}: a profile needs `index = True`, since an image without the layer sidecars is extracted rather than served".format(ctx.label))

    checks = []
    for profile in profiles:
        checked = ctx.actions.declare_file("{}.{}.checked".format(ctx.label.name, profile.basename))
        ctx.actions.run(
            executable = ctx.attr._tool[DefaultInfo].files_to_run,
            arguments = [
                "profile",
                "--profile",
                profile.path,
                "--layout",
                layout.path,
                "--index",
                indexes[0].path,
                "--stamp",
                checked.path,
            ],
            inputs = [layout, profile] + indexes,
            outputs = [checked],
            mnemonic = "OciProfileCheck",
            progress_message = "Checking " + profile.short_path + " against %{label}",
        )
        checks.append(checked)
    return profiles, checks

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
        "strict_xattrs": attr.bool(
            default = True,
            doc = """Fail rather than run an image whose layers set extended attributes.

Extended attributes are never restored. The one that matters in practice,
`security.capability`, needs a privilege the extraction does not have when it
runs rootless, so a container built from such an image does not match it: a
binary that expected a capability will not have one. Failing says so, rather
than leaving it to be discovered at runtime.

Switch it off to extract the image anyway, dropping the attributes.
""",
        ),
        "index": attr.bool(
            default = True,
            doc = """Index the layers at build time so the image can be served rather than extracted.

Each sidecar is a small file in the runfiles tree saying what a layer holds
and where inflating it can resume; the image layout and its digests are
unchanged. With one beside every layer the launcher can describe the whole
root filesystem without reading it, mount that, and fetch a file's bytes only
when something opens it. Without them it extracts the image, and where it does
extract the sidecars still let it do so in parallel.

Indexing decompresses every layer once per build of the image, so switch it
off if build time matters more than startup time.
""",
        ),
        "data": attr.label_list(
            allow_files = True,
            doc = "Extra files to make available in the runfiles tree, for use with `mounts`.",
        ),
        "profile": attr.label(
            providers = [OciRuntimeProfileInfo],
            doc = """An `oci_runtime_profile` naming what this container reads, fetched ahead of it.

A served image costs a wait every time the container opens a file nothing has
opened yet. A profile is the list of those files from a previous run, so they
can be fetched before it asks and on more than one thread at a time. Requires
`index = True`, since an image without the sidecars is extracted rather than
served.

Profiles are recorded by running the container, not by building it: see
`oci_runtime_profile`.
""",
        ),
        "_tool": attr.label(
            default = ":current_launcher",
            executable = True,
            cfg = "exec",
        ),
    },
    toolchains = [
        LAUNCHER_TOOLCHAIN_TYPE,
        CONTAINER_RUNTIME_TOOLCHAIN_TYPE,
    ],
)
