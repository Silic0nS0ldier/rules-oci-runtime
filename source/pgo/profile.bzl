"""A string build setting naming the profile, without depending on skylib."""

def _pgo_profile_impl(_ctx):
    return []

pgo_profile = rule(
    implementation = _pgo_profile_impl,
    build_setting = config.string(flag = True),
    doc = "Absolute path to a `.profdata` file, or empty to build without one.",
)
