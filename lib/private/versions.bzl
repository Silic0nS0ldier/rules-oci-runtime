"""Prebuilt launcher release pinned by the `launcher` module extension.

Stamped by `.github/workflows/make_release_archive.sh` when a release archive is
built. Empty hashes mean no release to download from, which is the case for a
source checkout and for the self-contained archives CI builds.
"""

visibility(["//lib/..."])

LAUNCHER_VERSION = "0.0.0"

LAUNCHER_SHA256 = {
    "linux_amd64": "",
    "linux_arm64": "",
}
