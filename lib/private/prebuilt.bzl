"""Toolchains for the launcher binaries baked into a self-contained archive."""

load(":toolchains.bzl", "launcher_toolchain")

visibility(["//launcher"])

_CONSTRAINTS = {
    "amd64": ["@platforms//os:linux", "@platforms//cpu:x86_64"],
    "arm64": ["@platforms//os:linux", "@platforms//cpu:arm64"],
}

def prebuilt_launcher_toolchains(binaries):
    """Declares a toolchain for each launcher binary present.

    Args:
        binaries: Launcher binaries, named `oci_runtime.<arch>`.
    """
    for binary in binaries:
        arch = binary.split(".")[-1]
        launcher_toolchain(
            name = "launcher_{}".format(arch),
            binary = binary,
        )
        native.toolchain(
            name = arch,
            target_compatible_with = _CONSTRAINTS[arch],
            # Opt out with `--@rules_oci_runtime//lib:prebuilt_launcher=false`,
            # which is how `rules_oci_runtime_source` gets to supply the
            # launcher instead.
            target_settings = ["//lib:prebuilt_launcher_enabled"],
            toolchain = ":launcher_{}".format(arch),
            toolchain_type = "//lib:launcher_toolchain_type",
        )
