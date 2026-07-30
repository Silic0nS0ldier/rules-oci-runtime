"""Module extension providing the default `runc` container runtime toolchains."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")

# Hashes are published as `runc.sha256sum` with each release.
_RUNC_VERSION = "1.3.0"

_RUNC_PLATFORMS = {
    "linux_amd64": struct(
        asset = "runc.amd64",
        sha256 = "028986516ab5646370edce981df2d8e8a8d12188deaf837142a02097000ae2f2",
        constraints = ["@platforms//os:linux", "@platforms//cpu:x86_64"],
    ),
    "linux_arm64": struct(
        asset = "runc.arm64",
        sha256 = "85c5e4e4f72e442c8c17bac07527cd4f961ee48e4f2b71797f7533c94f4a52b9",
        constraints = ["@platforms//os:linux", "@platforms//cpu:arm64"],
    ),
}

_HUB_NAME = "runc"

_HUB_HEADER = """\
load("@rules_oci_runtime//lib:defs.bzl", "container_runtime_toolchain")
"""

_HUB_PLATFORM = """
container_runtime_toolchain(
    name = "{platform}",
    binary = "@{hub}_{platform}//file",
)

toolchain(
    name = "{platform}_toolchain",
    target_compatible_with = {constraints},
    toolchain = ":{platform}",
    toolchain_type = "@rules_oci_runtime//lib:container_runtime_toolchain_type",
)
"""

def _hub_impl(repository_ctx):
    content = [_HUB_HEADER]
    for platform, info in _RUNC_PLATFORMS.items():
        content.append(_HUB_PLATFORM.format(
            constraints = repr(info.constraints),
            hub = _HUB_NAME,
            platform = platform,
        ))
    repository_ctx.file("BUILD.bazel", "".join(content))

_hub = repository_rule(implementation = _hub_impl)

def _runc_impl(module_ctx):
    for platform, info in _RUNC_PLATFORMS.items():
        http_file(
            name = "{}_{}".format(_HUB_NAME, platform),
            executable = True,
            sha256 = info.sha256,
            url = "https://github.com/opencontainers/runc/releases/download/v{}/{}".format(
                _RUNC_VERSION,
                info.asset,
            ),
        )
    _hub(name = _HUB_NAME)
    return module_ctx.extension_metadata(reproducible = True)

runc = module_extension(
    implementation = _runc_impl,
    doc = "Downloads a pinned `runc` release and declares a toolchain for each platform.",
)
