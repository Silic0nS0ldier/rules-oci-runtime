"""Profile-guided optimisation for the launcher and everything it links.

Most of the launcher's instructions are spent inside `zlib-rs`, which Bazel
compiles as its own `rustc` action, so `-Cprofile-use` on the launcher target
alone reaches almost none of the work: measured that way the profile bought
0.08%, against 6.2% when every crate in the graph saw it.

The flag therefore has to travel with the configuration rather than with the
target, and it has to stop at the launcher. The benchmark tools are built
`--config=release` as well and this profile does not describe them, so a
global flag would optimise them against a stranger and rebuild them for it.
"""

load("@rules_rust//rust:defs.bzl", "rust_binary")

_EXTRA_RUSTC_FLAG = "@rules_rust//rust/settings:extra_rustc_flag"
_PROFILE = "//pgo:profile"

def _profiled_impl(settings, _attr):
    # type: (dict, struct) -> dict
    flags = settings[_EXTRA_RUSTC_FLAG]
    profile = settings[_PROFILE]
    if profile:
        flags = flags + [
            # `rustc` does not run from the directory Bazel was invoked from,
            # so the profile is named absolutely, as the rustc book requires.
            "-Cprofile-use=" + profile,
            # LLVM is silent when a function has no profile data, which is the
            # only symptom of a profile that stopped describing the code.
            "-Cllvm-args=-pgo-warn-missing-function",
        ]
    return {_EXTRA_RUSTC_FLAG: flags}

_profiled = transition(
    implementation = _profiled_impl,
    inputs = [_EXTRA_RUSTC_FLAG, _PROFILE],
    outputs = [_EXTRA_RUSTC_FLAG],
)

def _launcher_binary_impl(ctx):
    # type: (ctx) -> list
    return ctx.super()

# A `rust_binary` under one more transition. The parent's own transition still
# runs, so the `platform` attribute keeps working.
launcher_binary = rule(
    implementation = _launcher_binary_impl,
    parent = rust_binary,
    cfg = _profiled,
    doc = "The launcher, built against `//pgo:profile` when one is configured.",
)
