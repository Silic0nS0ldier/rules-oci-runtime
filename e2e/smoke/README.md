# Smoke tests

End to end tests that pull a real image and run it through `runc_binary`.

```sh
bazel test //...
```

Each test case lives in [cases.sh](cases.sh) as a `case_<name>` function and is
exposed as a separate `sh_test` target.

Most cases run the pulled `alpine` image, whose layers are gzip. `zstd_image`
puts a `.tar.zst` layer on top of it with `tar` and `oci_image`, which reads the
compression off the archive and writes a `tar+zstd` layer: one image of both
compressions, and the only zstd blob here that a Rust crate did not produce.

```sh
bazel run //:container -- /bin/sh -c 'echo "Hello, world!"'
```
