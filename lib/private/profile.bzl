"""Profiles of what a container reads, recorded outside Bazel."""

visibility(["//lib/...", "//docs/..."])

OciRuntimeProfileInfo = provider(
    doc = "Recorded profiles of what a container read, and where new recordings go.",
    fields = {
        "files": "The profile files, one per platform. Possibly none of them.",
        "record_to": "Workspace relative base path a recording writes to, without the platform qualifier or suffix.",
    },
)

_SUFFIX = ".profile"

_DOC = """Collects the profiles a `runc_binary` fetches ahead from.

A profile says which files of an image a container actually read, so the next
run can fetch them before it asks rather than pay for each one as it blocks.
What a container reads is not something a build can work out, so profiles are
recorded by running the container:

```sh
bazel run //:container -- --record-profile /bin/sh -c 'echo hello'
```

That writes `<record_to>.<os>-<arch>.profile` into the source tree, merging
into whatever is there already, so recording several runs of a container
accumulates what they all read. The platform is part of the name because the
run only saw one manifest of the image, and another platform's would hold
other files.

Profiles are ordinary source files. They may not exist yet -- which is how the
first recording gets to happen -- so glob for them:

```starlark
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

`runc_binary` checks each profile against the image it is used with, so one
left behind by a rename or recorded against another image fails the build
rather than quietly fetching nothing.
"""

def _oci_runtime_profile_impl(ctx):
    # type: (ctx) -> list
    record_to = ctx.attr.record_to or ctx.label.name
    base = record_to.rsplit("/", 1)[-1]
    for src in ctx.files.srcs:
        name = src.basename
        if not name.endswith(_SUFFIX):
            fail("{}: {} is not named like a profile, which ends in `{}`".format(
                ctx.label,
                src.short_path,
                _SUFFIX,
            ))
        qualified = name[:-len(_SUFFIX)]
        parts = qualified.rsplit(".", 1)
        if len(parts) != 2 or "-" not in parts[1]:
            fail("{}: {} does not name a platform, expected `{}.<os>-<arch>{}`".format(
                ctx.label,
                src.short_path,
                base,
                _SUFFIX,
            ))
        if parts[0] != base:
            fail("{}: {} is not a profile of `{}`, which is where recordings go".format(
                ctx.label,
                src.short_path,
                record_to,
            ))

    return [
        DefaultInfo(files = depset(ctx.files.srcs)),
        OciRuntimeProfileInfo(
            files = ctx.files.srcs,
            record_to = "{}/{}".format(ctx.label.package, record_to) if ctx.label.package else record_to,
        ),
    ]

oci_runtime_profile = rule(
    implementation = _oci_runtime_profile_impl,
    doc = _DOC,
    attrs = {
        "srcs": attr.label_list(
            allow_files = [_SUFFIX],
            doc = """Recorded profiles, named `<record_to>.<os>-<arch>.profile`.

Empty is allowed and means nothing has been recorded yet: the container runs
as it always has, and recording one is what fills this in.
""",
        ),
        "record_to": attr.string(
            doc = """Package relative base path a recording writes to, defaulting to the target name.

The platform and the `.profile` suffix are added by the recording, so
`profiles/container` records `profiles/container.linux-amd64.profile`.
""",
        ),
    },
)
