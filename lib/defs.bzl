"""Bazel rules for running OCI container images."""

load("//lib/private:profile.bzl", _oci_runtime_profile = "oci_runtime_profile")
load("//lib/private:runc_binary.bzl", _runc_binary = "runc_binary")
load(
    "//lib/private:toolchains.bzl",
    _container_runtime_toolchain = "container_runtime_toolchain",
    _launcher_toolchain = "launcher_toolchain",
)

runc_binary = _runc_binary
oci_runtime_profile = _oci_runtime_profile
launcher_toolchain = _launcher_toolchain
container_runtime_toolchain = _container_runtime_toolchain
