load("@jq.bzl//jq/toolchain:toolchain.bzl", "TOOLCHAIN_TYPE")

visibility(["//lib/..."])

def _jq_binary_impl(ctx):
    # type: (ctx) -> None
    toolchain_info = ctx.toolchains[TOOLCHAIN_TYPE]
    jq_bin = toolchain_info.jqinfo.bin
    
    # Symlink for consistent rlocation path
    ctx.actions.symlink(
        target_file = jq_bin,
        output = ctx.outputs.executable,
        is_executable = True,
    )
    runfiles = ctx.runfiles(files = [jq_bin])

    return [
        DefaultInfo(
            executable = ctx.outputs.executable,
            runfiles = runfiles,
        )
    ]

jq_binary = rule(
    implementation = _jq_binary_impl,
    toolchains = [TOOLCHAIN_TYPE],
    executable = True,
)

