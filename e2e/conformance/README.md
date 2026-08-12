# Conformance tests

End to end tests that extract an image with the launcher and with
[umoci](https://github.com/opencontainers/umoci), and compare the two trees.

```sh
bazel test //...
```

The unit tests in [`source`](../../source) hold the launcher's extraction routes
to each other and to the spec as we have read it. These hold them to an
implementation someone else read, which is the only way to catch a clause we
have misread the same way twice.

umoci is downloaded as a pinned binary, so nothing here needs a Go toolchain.
Both sides run rootless, so uid and gid are not compared: neither can set them.

Extended attributes are not compared either, but for a different reason. Neither
side can restore `security.capability` without privileges we do not have, so
they agree there. umoci does restore `user.*` attributes and the launcher
restores nothing, so they disagree there. No image measured carries a `user.*`
attribute, and the launcher's side of that is pinned by a unit test rather than
here.

Each case lives in [cases.sh](cases.sh) as a `case_<name>` function and is
exposed as a separate `sh_test` target. A case builds its layers with `tar`,
assembles them into an image with `umoci raw add-layer`, then compares the
entries, their contents, and which names share an inode.

Layers are named `[<compression>:]<path>`, defaulting to gzip. Both formats
carry checkpoints, so every fixture is extracted by the walk and then by the
parallel route; each case asserts that it got one sidecar per layer.

Every fixture ships an `etc/` directory. The launcher writes `etc/hostname`,
`etc/hosts` and `etc/resolv.conf` into the bundle itself, and those three are
excluded from the comparison; without an `etc/` in the layer the launcher would
create the directory too, which is a difference the fixture caused rather than
the extractor.
