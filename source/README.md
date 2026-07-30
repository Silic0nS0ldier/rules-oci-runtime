# `rules_oci_runtime_source`

The launcher for [`rules_oci_runtime`](../README.md): a single Rust binary that
unpacks an OCI image layout into a bundle and runs it through an OCI runtime
such as `runc`.

This lives in its own Bazel module so that building the launcher from source,
and therefore depending on `rules_rust`, stays opt in. Add it alongside the
rules to register a launcher toolchain:

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime", version = "0.0.0")
bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")
```

See the top-level [README](../README.md#how-it-works) for what the launcher
does at run time.
