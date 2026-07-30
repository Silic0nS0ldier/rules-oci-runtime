"""Bazel rules for running OCI container images."""

load("//lib/private:runc_binary.bzl", _runc_binary = "runc_binary")

runc_binary = _runc_binary
