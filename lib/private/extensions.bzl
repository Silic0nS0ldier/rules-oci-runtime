"""Module extensions providing the default launcher and container runtime toolchains."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")

_CONSTRAINTS = {
    "linux_amd64": ["@platforms//os:linux", "@platforms//cpu:x86_64"],
    "linux_arm64": ["@platforms//os:linux", "@platforms//cpu:arm64"],
}

# Hashes are published as `runc.sha256sum` with each release.
_RUNC_VERSION = "1.3.0"

_RUNC_PLATFORMS = {
    "linux_amd64": struct(
        asset = "runc.amd64",
        sha256 = "028986516ab5646370edce981df2d8e8a8d12188deaf837142a02097000ae2f2",
    ),
    "linux_arm64": struct(
        asset = "runc.arm64",
        sha256 = "85c5e4e4f72e442c8c17bac07527cd4f961ee48e4f2b71797f7533c94f4a52b9",
    ),
}

# Tracks the release of these rules, hashes are published as
# `oci_runtime.sha256sum` with each one.
_LAUNCHER_VERSION = "0.0.0"

_LAUNCHER_PLATFORMS = {
    "linux_amd64": struct(
        asset = "oci_runtime.amd64",
        sha256 = "",
    ),
    "linux_arm64": struct(
        asset = "oci_runtime.arm64",
        sha256 = "",
    ),
}

_HUB_HEADER = """\
load("@rules_oci_runtime//lib:defs.bzl", "{rule}")
"""

_HUB_PLATFORM = """
{rule}(
    name = "{platform}",
    binary = "@{hub}_{platform}//file",
)

toolchain(
    name = "{platform}_toolchain",
    target_compatible_with = {constraints},
    target_settings = {target_settings},
    toolchain = ":{platform}",
    toolchain_type = "@rules_oci_runtime//lib:{toolchain_type}",
)
"""

def _hub_impl(repository_ctx):
    content = [_HUB_HEADER.format(rule = repository_ctx.attr.toolchain_rule)]
    for platform in repository_ctx.attr.platforms:
        content.append(_HUB_PLATFORM.format(
            constraints = repr(_CONSTRAINTS[platform]),
            hub = repository_ctx.attr.hub,
            platform = platform,
            rule = repository_ctx.attr.toolchain_rule,
            target_settings = repr(repository_ctx.attr.target_settings),
            toolchain_type = repository_ctx.attr.toolchain_type,
        ))
    repository_ctx.file("BUILD.bazel", "".join(content))

_hub = repository_rule(
    implementation = _hub_impl,
    attrs = {
        # Apparent name of the hub, `repository_ctx.attr.name` is canonicalised.
        "hub": attr.string(),
        "platforms": attr.string_list(),
        "target_settings": attr.string_list(),
        "toolchain_rule": attr.string(),
        "toolchain_type": attr.string(),
    },
)

def _runc_impl(module_ctx):
    for platform, info in _RUNC_PLATFORMS.items():
        http_file(
            name = "runc_{}".format(platform),
            executable = True,
            sha256 = info.sha256,
            url = "https://github.com/opencontainers/runc/releases/download/v{}/{}".format(
                _RUNC_VERSION,
                info.asset,
            ),
        )
    _hub(
        name = "runc",
        hub = "runc",
        platforms = _RUNC_PLATFORMS.keys(),
        toolchain_rule = "container_runtime_toolchain",
        toolchain_type = "container_runtime_toolchain_type",
    )
    return module_ctx.extension_metadata(reproducible = True)

runc = module_extension(
    implementation = _runc_impl,
    doc = "Downloads a pinned `runc` release and declares a toolchain for each platform.",
)

def _launcher_impl(module_ctx):
    for platform, info in _LAUNCHER_PLATFORMS.items():
        http_file(
            name = "launcher_{}".format(platform),
            executable = True,
            sha256 = info.sha256,
            url = "https://github.com/Silic0nS0ldier/rules-oci-runtime/releases/download/v{}/{}".format(
                _LAUNCHER_VERSION,
                info.asset,
            ),
        )
    _hub(
        name = "launcher",
        hub = "launcher",
        platforms = _LAUNCHER_PLATFORMS.keys(),
        # Opt out with `--@rules_oci_runtime//lib:prebuilt_launcher=false`, which
        # is how `rules_oci_runtime_source` gets to provide the launcher instead.
        target_settings = ["@rules_oci_runtime//lib:prebuilt_launcher_enabled"],
        toolchain_rule = "launcher_toolchain",
        toolchain_type = "launcher_toolchain_type",
    )
    return module_ctx.extension_metadata(reproducible = True)

launcher = module_extension(
    implementation = _launcher_impl,
    doc = "Downloads a pinned launcher release and declares a toolchain for each platform.",
)
