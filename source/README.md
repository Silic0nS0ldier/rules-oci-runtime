# `rules_oci_runtime_source`

The launcher for [`rules_oci_runtime`](../README.md): a single Rust binary that
unpacks an OCI image layout into a bundle and runs it through an OCI runtime
such as `runc`.

This lives in its own Bazel module so that building the launcher from source,
and therefore depending on `rules_rust`, stays opt in. `rules_oci_runtime` uses
a prebuilt launcher by default, so switch that off when adding this module:

```starlark
# MODULE.bazel
bazel_dep(name = "rules_oci_runtime", version = "0.0.0")
bazel_dep(name = "rules_oci_runtime_source", version = "0.0.0")
```

```
# .bazelrc
build --@rules_oci_runtime//lib:prebuilt_launcher=false
```

See the top-level [README](../README.md#how-it-works) for what the launcher
does at run time.

## Benchmarking

Two tools, built by Bazel and run outside it. Running them through `bazel run`
would measure the sandbox as much as the launcher, so build first and invoke
the binaries directly:

```
bazel build //:bench_image //:bench_run
.bazel/bin/bench_image --output /tmp/bench-full --profile full
.bazel/bin/bench_run --layout /tmp/bench-full --rounds 7 old/oci_runtime new/oci_runtime
```

`bench_image` writes a deterministic image layout. The seed is the whole
reproduction: the same seed and profile give byte identical blobs. The `full`
profile is shaped like the image this launcher is actually slow on -- a long
tail of small files with a few very large ones holding most of the bytes,
seven thousand directories, and three bytes shadowed by a later layer for every
two that survive. A distribution base image has none of that, and a per entry
regression that was invisible on alpine once cost 5x on a real image.

`bench_run` compares binaries against one layout. It builds each binary's own
sidecars, reports the route each one took and refuses to compare two that
disagree, discards a warmup, interleaves the timed rounds and reverses their
order every other round. It reports counts beside the times, and says "within
noise" rather than printing a number too small to attribute.

Pass `--syscalls` for a separate `strace -c` pass; syscall counts do not move
when the host is busy, so a difference of one is a difference. `--perf` adds
instructions retired where the host allows it.

Passing the same binary twice is how to find out what this host's floor is
today:

```
.bazel/bin/bench_run --layout /tmp/bench-full --rounds 7 \
    .bazel/bin/oci_runtime .bazel/bin/oci_runtime
```

`//:bench_counts_test` holds the same counts in CI, where the clock is worth
nothing: entries placed, what the plan skipped, and the syscalls each route
makes. Its numbers come from `bench_image --profile small`; when the generator
changes, run the test and update them from what it reports.

### The served route

`bench_run` measures extraction, and has no served mode: what a served rootfs
costs depends on what the container reads, which is not something the harness
knows. Until it has one, run the launcher against a generated layout with a
stand-in runtime that reads part of the tree, and read the count the launcher
reports itself:

```
.bazel/bin/oci_runtime run --layout /tmp/bench-full --index /tmp/idx-full \
    --rootfs=fuse --runtime ./reads-some-files.sh --verbose 2>&1 | grep waited
```

`The container waited for N files to be fetched` is the number to compare. It
is the count of opens that had to inflate a span before they could be answered,
so it does not move with the host the way the clock does, and it is what a
profile is meant to drive to zero.

## Profile-guided optimisation

Release launchers are built against `pgo/oci_runtime.profdata`, which is
committed. Most of the launcher's time is inside `zlib-rs`, and the profile
tells LLVM which way its branches actually go: it takes about 15% of the
instructions out of `inflate`.

`rustc` resolves the path itself, from a working directory that is neither the
workspace nor stable, so it has to be absolute and the caller supplies it:

```
bazel build --config=release \
    --//pgo:profile=$PWD/pgo/oci_runtime.profdata //:oci_runtime
```

Leave it out and the launcher still builds, just slower. The flag travels by a
transition attached to `//:oci_runtime`, so it reaches every crate the launcher
links and nothing else: the benchmark tools are not built against a profile
that does not describe them.

Regenerate the profile when the extraction path changes shape. A stale profile
is not wrong -- it just stops paying for the functions it no longer describes,
and `-pgo-warn-missing-function`, which the transition always passes, says
which those are.

```
# 1. Instrument. Delete the old data first: `.profraw` files are updated in
#    place, so one left over from another binary poisons the merge.
rm -rf /tmp/pgo-data
bazel build --config=release \
    --@rules_rust//rust/settings:extra_rustc_flag=-Cstrip=symbols \
    --@rules_rust//rust/settings:extra_rustc_flag=-Cprofile-generate=/tmp/pgo-data \
    //:oci_runtime

# 2. Train, on both routes: with sidecars and without.
bazel build //:bench_image
.bazel/bin/bench_image --output /tmp/bench-full --profile full
.bazel/bin/oci_runtime index --layout /tmp/bench-full --output /tmp/idx-full
for index in "--index /tmp/idx-full" ""; do
    .bazel/bin/oci_runtime run --layout /tmp/bench-full $index \
        --runtime /nonexistent/runc --keep-bundle
done

# 3. Merge. `llvm-profdata` has to be the one matching rustc's LLVM.
"$(find "$(bazel info output_base)/external" -name llvm-profdata | head -1)" \
    merge -o pgo/oci_runtime.profdata /tmp/pgo-data/*.profraw
```

The instrumented binary is much slower than either; never benchmark it.
