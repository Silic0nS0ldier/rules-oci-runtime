# Smoke tests

End to end tests that pull a real image and run it through `runc_binary`.

```sh
bazel test //...
```

Each test case lives in [cases.sh](cases.sh) as a `case_<name>` function and is
exposed as a separate `sh_test` target.

```sh
bazel run //:container -- /bin/sh -c 'echo "Hello, world!"'
```
