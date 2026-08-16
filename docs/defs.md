<!-- Generated with Stardoc: http://skydoc.bazel.build -->

Bazel rules for running OCI container images.

<a id="container_runtime_toolchain"></a>

## container_runtime_toolchain

<pre>
load("@rules_oci_runtime//lib:defs.bzl", "container_runtime_toolchain")

container_runtime_toolchain(<a href="#container_runtime_toolchain-name">name</a>, <a href="#container_runtime_toolchain-binary">binary</a>)
</pre>

Declares the OCI runtime that executes a bundle, such as `runc`.

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

**ATTRIBUTES**


| Name  | Description | Type | Mandatory | Default |
| :------------- | :------------- | :------------- | :------------- | :------------- |
| <a id="container_runtime_toolchain-name"></a>name |  A unique name for this target.   | <a href="https://bazel.build/concepts/labels#target-names">Name</a> | required |  |
| <a id="container_runtime_toolchain-binary"></a>binary |  The container runtime executable.   | <a href="https://bazel.build/concepts/labels">Label</a> | required |  |


<a id="launcher_toolchain"></a>

## launcher_toolchain

<pre>
load("@rules_oci_runtime//lib:defs.bzl", "launcher_toolchain")

launcher_toolchain(<a href="#launcher_toolchain-name">name</a>, <a href="#launcher_toolchain-binary">binary</a>)
</pre>

Declares the launcher that `runc_binary` targets execute.

The launcher reads an OCI image layout, extracts it into a bundle and hands
that bundle to the container runtime. Every release archive comes with a
prebuilt launcher toolchain for Linux amd64 and arm64. A source checkout comes
with none, so it must supply the launcher by one of the means below.

To build it from source instead, which pulls in `rules_rust`, take the prebuilt
toolchains out of the running and add the module that provides it:

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")
```

```
# .bazelrc
build --@rules_oci_runtime//lib:prebuilt_launcher=false
```

Register a `launcher_toolchain` to use a patched launcher instead.

**ATTRIBUTES**


| Name  | Description | Type | Mandatory | Default |
| :------------- | :------------- | :------------- | :------------- | :------------- |
| <a id="launcher_toolchain-name"></a>name |  A unique name for this target.   | <a href="https://bazel.build/concepts/labels#target-names">Name</a> | required |  |
| <a id="launcher_toolchain-binary"></a>binary |  The launcher executable.   | <a href="https://bazel.build/concepts/labels">Label</a> | required |  |


<a id="oci_runtime_profile"></a>

## oci_runtime_profile

<pre>
load("@rules_oci_runtime//lib:defs.bzl", "oci_runtime_profile")

oci_runtime_profile(<a href="#oci_runtime_profile-name">name</a>, <a href="#oci_runtime_profile-srcs">srcs</a>, <a href="#oci_runtime_profile-record_to">record_to</a>)
</pre>

Collects the profiles a `runc_binary` fetches ahead from.

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

**ATTRIBUTES**


| Name  | Description | Type | Mandatory | Default |
| :------------- | :------------- | :------------- | :------------- | :------------- |
| <a id="oci_runtime_profile-name"></a>name |  A unique name for this target.   | <a href="https://bazel.build/concepts/labels#target-names">Name</a> | required |  |
| <a id="oci_runtime_profile-srcs"></a>srcs |  Recorded profiles, named `<record_to>.<os>-<arch>.profile`.<br><br>Empty is allowed and means nothing has been recorded yet: the container runs as it always has, and recording one is what fills this in.   | <a href="https://bazel.build/concepts/labels">List of labels</a> | optional |  `[]`  |
| <a id="oci_runtime_profile-record_to"></a>record_to |  Package relative base path a recording writes to, defaulting to the target name.<br><br>The platform and the `.profile` suffix are added by the recording, so `profiles/container` records `profiles/container.linux-amd64.profile`.   | String | optional |  `""`  |


<a id="runc_binary"></a>

## runc_binary

<pre>
load("@rules_oci_runtime//lib:defs.bzl", "runc_binary")

runc_binary(<a href="#runc_binary-name">name</a>, <a href="#runc_binary-data">data</a>, <a href="#runc_binary-env">env</a>, <a href="#runc_binary-hostname">hostname</a>, <a href="#runc_binary-image">image</a>, <a href="#runc_binary-index">index</a>, <a href="#runc_binary-mounts">mounts</a>, <a href="#runc_binary-profile">profile</a>, <a href="#runc_binary-read_only">read_only</a>, <a href="#runc_binary-strict_xattrs">strict_xattrs</a>,
            <a href="#runc_binary-workdir">workdir</a>)
</pre>

Creates an executable that runs an OCI image with `runc`.

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

**ATTRIBUTES**


| Name  | Description | Type | Mandatory | Default |
| :------------- | :------------- | :------------- | :------------- | :------------- |
| <a id="runc_binary-name"></a>name |  A unique name for this target.   | <a href="https://bazel.build/concepts/labels#target-names">Name</a> | required |  |
| <a id="runc_binary-data"></a>data |  Extra files to make available in the runfiles tree, for use with `mounts`.   | <a href="https://bazel.build/concepts/labels">List of labels</a> | optional |  `[]`  |
| <a id="runc_binary-env"></a>env |  Environment variables added to the container, overriding the image.   | <a href="https://bazel.build/rules/lib/core/dict">Dictionary: String -> String</a> | optional |  `{}`  |
| <a id="runc_binary-hostname"></a>hostname |  Hostname inside the container. Defaults to `container`.   | String | optional |  `""`  |
| <a id="runc_binary-image"></a>image |  An OCI image layout directory, such as an `oci_image` or `oci.pull` target.   | <a href="https://bazel.build/concepts/labels">Label</a> | required |  |
| <a id="runc_binary-index"></a>index |  Index the layers at build time so the image can be served rather than extracted.<br><br>Each sidecar is a small file in the runfiles tree saying what a layer holds and where inflating it can resume; the image layout and its digests are unchanged. With one beside every layer the launcher can describe the whole root filesystem without reading it, mount that, and fetch a file's bytes only when something opens it. Without them it extracts the image, and where it does extract the sidecars still let it do so in parallel.<br><br>Indexing decompresses every layer once per build of the image, so switch it off if build time matters more than startup time.   | Boolean | optional |  `True`  |
| <a id="runc_binary-mounts"></a>mounts |  Bind mounts, each `SOURCE:DESTINATION[:OPTIONS]`.<br><br>`OPTIONS` is a comma separated mount option list such as `ro` or `rw,noexec`, defaulting to `rw`. `$VAR` and `${VAR}` are expanded in `SOURCE` when the container starts, so `$BUILD_WORKSPACE_DIRECTORY:/src:ro` mounts the workspace.   | List of strings | optional |  `[]`  |
| <a id="runc_binary-profile"></a>profile |  An `oci_runtime_profile` naming what this container reads, fetched ahead of it.<br><br>A served image costs a wait every time the container opens a file nothing has opened yet. A profile is the list of those files from a previous run, so they can be fetched before it asks and on more than one thread at a time. Requires `index = True`, since an image without the sidecars is extracted rather than served.<br><br>Profiles are recorded by running the container, not by building it: see `oci_runtime_profile`.   | <a href="https://bazel.build/concepts/labels">Label</a> | optional |  `None`  |
| <a id="runc_binary-read_only"></a>read_only |  Mount the container root filesystem read-only.   | Boolean | optional |  `False`  |
| <a id="runc_binary-strict_xattrs"></a>strict_xattrs |  Fail rather than run an image whose layers set extended attributes.<br><br>Extended attributes are never restored. The one that matters in practice, `security.capability`, needs a privilege the extraction does not have when it runs rootless, so a container built from such an image does not match it: a binary that expected a capability will not have one. Failing says so, rather than leaving it to be discovered at runtime.<br><br>Switch it off to extract the image anyway, dropping the attributes.   | Boolean | optional |  `True`  |
| <a id="runc_binary-workdir"></a>workdir |  Working directory inside the container, overriding the image `WorkingDir`.   | String | optional |  `""`  |


