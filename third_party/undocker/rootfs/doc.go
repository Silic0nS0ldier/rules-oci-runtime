// Package rootfs extracts all layers of a Docker container image to a single
// tarball. It will go trough all layers in order and copy every file to the
// destination archive.
//
// It will also reasonably process those files.
//
// == Non-directory will be copied only once ==
//
// A non-directory will be copied only once, only from within it's past
// occurrence. I.e. if file /a/b was found in layers 0 and 2, only the file
// from layer 2 will be used.
// Directories will always be copied, even if there are duplicates. This is
// to avoid a situation like this:
//   layer0:
//     ./dir/
//     ./dir/file
//   layer1:
//     ./dir/
//     ./dir/file
// In theory, the directory from layer 1 takes precedence, so a tarball like
// this could be created:
//   ./dir/      (from layer1)
//   ./dir/file1 (from layer1)
// However, imagine the following:
//   layer0:
//     ./dir/
//     ./dir/file1
//   layer1:
//     ./dir/
// Then the resulting tarball would have:
//   ./dir/file1 (from layer1)
//   ./dir/      (from layer0)
// Which would mean `untar` would try to untar a file to a directory which
// was not yet created. Therefore directories will be copied to the resulting
// tar in the order they appear in the layers.
//
// == Special files: .dockerenv ==
//
// .dockernv is present in all docker containers, and is likely to remain
// such. So if you do `docker export <container>`, the resulting tarball will
// have this file. rootfs will not add it. You are welcome to append one
// yourself.
//
// == Special files: opaque files and dirs (.wh.*) ==
//
// From mount.aufs(8)[1]:
//
//   The whiteout is for hiding files on lower branches. Also it is applied to
//   stop readdir going lower branches. The latter case is called ‘opaque
//   directory.’ Any whiteout is an empty file, it means whiteout is just an
//   mark. In the case of hiding lower files, the name of whiteout is
//   ‘.wh.<filename>.’ And in the case of stopping readdir, the name is
//   ‘.wh..wh..opq’. All whiteouts are hardlinked, including ‘<writable branch
//   top dir>/.wh..wh.aufs`.
//
// My interpretation:
//
// 1. a file/hardlink called `.wh..wh..opq` means that directory contents from
// the layers below the mentioned file should be ignored. Higher layers may add
// files on top. Ambiguity: should the directory from the lower layers be
// removed? I am assuming yes, but this assumptions is baseless.
//
// 2. if file/hardlink `.wh.([^/]+)` is found, $1 should be deleted from the
// current and lower layers.
//
// Note: these may be regular files in practice. So this implementation will
// match either.
//
// == Tar format ==
//
// Since we do care about long filenames and large file sizes (>8GB), we are
// using "classic" GNU Tar. However, at least NetBSD pax is known to have
// problems reading it[2].
//
// [1]: https://manpages.debian.org/unstable/aufs-tools/mount.aufs.8.en.html
//
// [2]: https://mgorny.pl/articles/portability-of-tar-features.html
package rootfs
