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


<a id="runc_binary"></a>

## runc_binary

<pre>
load("@rules_oci_runtime//lib:defs.bzl", "runc_binary")

runc_binary(<a href="#runc_binary-name">name</a>, <a href="#runc_binary-data">data</a>, <a href="#runc_binary-env">env</a>, <a href="#runc_binary-hostname">hostname</a>, <a href="#runc_binary-image">image</a>, <a href="#runc_binary-mounts">mounts</a>, <a href="#runc_binary-read_only">read_only</a>, <a href="#runc_binary-workdir">workdir</a>)
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
| <a id="runc_binary-mounts"></a>mounts |  Bind mounts, each `SOURCE:DESTINATION[:OPTIONS]`.<br><br>`OPTIONS` is a comma separated mount option list such as `ro` or `rw,noexec`, defaulting to `rw`. `$VAR` and `${VAR}` are expanded in `SOURCE` when the container starts, so `$BUILD_WORKSPACE_DIRECTORY:/src:ro` mounts the workspace.   | List of strings | optional |  `[]`  |
| <a id="runc_binary-read_only"></a>read_only |  Mount the container root filesystem read-only.   | Boolean | optional |  `False`  |
| <a id="runc_binary-workdir"></a>workdir |  Working directory inside the container, overriding the image `WorkingDir`.   | String | optional |  `""`  |


