//! Unpacking image layers into a root filesystem, replacing the previous
//! `undocker | tar -x` pipeline.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::image::{Descriptor, Layout, hex_encode, parse_digest};
use crate::log::{log, warning};
use crate::zinfo;

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Size of each buffer handed from the decompressor to the writer.
const CHUNK_BYTES: usize = 256 * 1024;

/// How many chunks may be in flight, bounding the pipeline to 2 MiB.
const PIPELINE_DEPTH: usize = 8;

/// A pipeline buffer and how much of it the producer filled.
///
/// The buffer keeps its full length for its whole life, so a recycled one is
/// handed straight back to `Read::read` without being zeroed again; `len` is
/// what makes the rest of it invisible.
struct Chunk {
    buf: Vec<u8>,
    len: usize,
}

impl std::ops::Deref for Chunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// The producer's end of a buffer pool.
///
/// A quarter of a megabyte is above the threshold at which an allocator stops
/// serving from its heap: musl sends every one of these to `mmap` and gives it
/// back on free, which for a 535 MiB layer is thousands of mappings and a
/// page fault for each of their pages. Circulating a handful of buffers costs
/// one channel operation instead.
struct Pool(Receiver<Vec<u8>>);

impl Pool {
    fn take(&self) -> Vec<u8> {
        self.0.try_recv().unwrap_or_else(|_| vec![0u8; CHUNK_BYTES])
    }
}

/// Creates a pool and the handle used to return buffers to it. Returns never
/// block, so the channel holds every buffer the pipeline can have in flight.
fn buffer_pool() -> (Pool, SyncSender<Vec<u8>>) {
    let (ret, free) = sync_channel(PIPELINE_DEPTH * 2);
    (Pool(free), ret)
}

/// Applies layers in order, deferring directory permissions so that read-only
/// directories in one layer do not block writes from the next.
pub struct RootfsExtractor {
    rootfs: Utf8PathBuf,
    index_dir: Option<Utf8PathBuf>,
    deferred_modes: Vec<(PathBuf, u32)>,
    parents: fsutil::ParentCache,
}

impl RootfsExtractor {
    pub fn new(rootfs: &Utf8Path, index_dir: Option<&Utf8Path>) -> Result<Self> {
        fs::create_dir_all(rootfs).io_context(|| format!("creating {rootfs}"))?;
        Ok(RootfsExtractor {
            parents: fsutil::ParentCache::new(rootfs.as_std_path())?,
            rootfs: rootfs.to_owned(),
            index_dir: index_dir.map(Utf8Path::to_owned),
            deferred_modes: Vec::new(),
        })
    }

    /// Decompression is CPU bound and writing the rootfs is IO bound, so a
    /// second thread inflates the blob while this one writes the entries. The
    /// two overlap rather than run back to back, which on a large layer is
    /// worth roughly the whole decompression time.
    ///
    /// With a checkpoint index the inflating side additionally spreads over
    /// the idle cores, since checkpoints let disjoint spans of one gzip
    /// member decompress independently.
    pub fn apply_layer(&mut self, layout: &Layout, descriptor: &Descriptor) -> Result<()> {
        let index = self.layer_index(descriptor);
        match &index {
            Some(index) => log!(
                "Extracting layer {} ({}) using {} checkpoints",
                descriptor.digest,
                descriptor.media_type,
                index.checkpoints.len()
            ),
            None => log!("Extracting layer {} ({})", descriptor.digest, descriptor.media_type),
        }

        let file = layout.open_blob(descriptor)?;
        let digest = descriptor.digest.clone();
        let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
        let (pool, ret) = buffer_pool();
        // Indexed spans are sized by the index rather than by CHUNK_BYTES, so
        // there is nothing for the streaming pool to hand them back to.
        let ret = index.is_none().then_some(ret);
        let owned = descriptor.clone();
        let inflate = thread::spawn(move || match index {
            Some(index) => inflate_indexed(file, &index, &owned, sender),
            None => inflate_blob(file, &owned, sender, pool),
        });

        let mut reader = ChunkReader::new(receiver, ret);
        let mut unpacked = self.unpack(&mut reader, &digest);
        if unpacked.is_ok() {
            // The tar stream ends at its marker, but the digest covers the blob.
            unpacked = io::copy(&mut reader, &mut io::sink())
                .map(|_| ())
                .io_context(|| format!("reading layer {digest}"));
        }
        // Releasing the receiver lets the inflater stop early when unpacking failed.
        drop(reader);

        match inflate.join() {
            // An unverified blob explains any unpacking failure, so it wins.
            Ok(inflated) => inflated.and(unpacked),
            Err(_) => Err(Error::io(
                "decompressing a layer",
                io::Error::other("the decompression thread panicked"),
            )),
        }
    }

    /// The checkpoint index for this layer, when there is one and parallel
    /// decompression can put it to use. An index is an optimisation, so
    /// anything wrong with it means falling back, not failing: the streaming
    /// path decides what the blob actually contains.
    fn layer_index(&self, descriptor: &Descriptor) -> Option<zinfo::Index> {
        let dir = self.index_dir.as_ref()?;
        if compression_of(&descriptor.media_type) != Some(Compression::Gzip) {
            return None;
        }
        if thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
            return None;
        }
        let hex = parse_digest(&descriptor.digest).ok()?.hex;
        let path = dir.join(format!("{hex}.zinfo"));
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                warning!("ignoring layer index {path}: {err}");
                return None;
            }
        };
        match zinfo::Index::read_from(io::BufReader::new(file)) {
            Ok(index) if index.checkpoints.len() > 1 => Some(index),
            Ok(_) => None,
            Err(err) => {
                warning!("ignoring layer index {path}: {err}");
                None
            }
        }
    }

    fn unpack(&mut self, reader: &mut dyn Read, layer: &str) -> Result<()> {
        let rootfs = self.rootfs.clone();
        let root = rootfs.as_std_path();
        let mut archive = tar::Archive::new(reader);
        archive.set_overwrite(true);
        let entries = archive
            .entries()
            .io_context(|| format!("reading layer {layer}"))?;

        // One buffer for the whole layer: allocating per file would cost more
        // than the copy it serves.
        let mut buffer = vec![0u8; CHUNK_BYTES];

        for entry in entries {
            let mut entry = entry.io_context(|| format!("reading layer {layer}"))?;
            let path = entry
                .path()
                .io_context(|| format!("reading entry path in layer {layer}"))?
                .into_owned();

            let Some(parts) = fsutil::sanitize_relative_path(&path) else {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            };

            let name = parts.last().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if name == OPAQUE_WHITEOUT {
                let dir = fsutil::join_components(root, &parts[..parts.len() - 1]);
                self.apply_opaque_whiteout(root, &dir)?;
                continue;
            }
            if let Some(target) = name.strip_prefix(WHITEOUT_PREFIX) {
                let mut whiteout = parts[..parts.len() - 1].to_vec();
                whiteout.push(target.into());
                let dst = fsutil::join_components(root, &whiteout);
                if self.parents.contains_parent_of(&dst)? {
                    log!("Whiteout: removing /{}", relative_display(root, &dst));
                    if fsutil::remove_any(&dst)? {
                        self.parents.forget(&dst);
                    }
                }
                continue;
            }

            let dst = fsutil::join_components(root, &parts);
            let entry_type = entry.header().entry_type();
            if !is_supported(entry_type) {
                warning!(
                    "skipping unsupported entry {:?} of type {:?} in layer {layer}",
                    path.display(),
                    entry_type
                );
                continue;
            }

            if !self.parents.prepare(&dst)? {
                return Err(Error::UnsafeEntry {
                    layer: layer.to_string(),
                    path: path.display().to_string(),
                });
            }

            let mode = entry.header().mode().unwrap_or(0o755) & 0o7777;

            // Regular files are the bulk of a layer, and tar copies them
            // through a buffer of std's default size, which is one write
            // syscall per 8 KiB. Ours is 32 times larger.
            if matches!(entry_type, EntryType::Regular | EntryType::Continuous) {
                entry.set_preserve_mtime(true);
                let replaced = unpack_regular(&mut entry, &dst, mode, &mut buffer)
                    .io_context(|| format!("extracting {:?} from layer {layer}", path.display()))?;
                if replaced {
                    self.parents.forget(&dst);
                }
                continue;
            }

            if entry_type.is_dir() {
                if prepare_directory(&dst)? {
                    self.parents.forget(&dst);
                }
                self.deferred_modes.push((dst.clone(), mode));
                match fs::create_dir(&dst) {
                    Ok(()) => {}
                    // `prepare_directory` cleared anything that was not a
                    // directory, so what is left is one to keep.
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => {
                        return Err(Error::io(format!("creating {}", dst.display()), err));
                    }
                }
                continue;
            }

            // Replacing whatever is here is what `set_overwrite` bought from
            // `tar`, and the link calls below will not do it themselves.
            if fsutil::remove_any(&dst)? {
                self.parents.forget(&dst);
            }

            let unsafe_entry = || Error::UnsafeEntry {
                layer: layer.to_string(),
                path: path.display().to_string(),
            };

            match entry_type {
                EntryType::Symlink => {
                    let target = link_name(&entry, layer)?.ok_or_else(unsafe_entry)?;
                    std::os::unix::fs::symlink(&target, &dst).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                    if let Ok(mtime) = entry.header().mtime() {
                        set_symlink_mtime(&dst, mtime);
                    }
                }
                EntryType::Link => {
                    let target = link_name(&entry, layer)?.ok_or_else(unsafe_entry)?;
                    // A hard link names an earlier entry of the same archive,
                    // so it is rooted at the rootfs like any other entry path.
                    let source = fsutil::sanitize_relative_path(&target)
                        .map(|parts| fsutil::join_components(root, &parts))
                        .ok_or_else(unsafe_entry)?;
                    if !self.parents.contains_parent_of(&source)? {
                        return Err(unsafe_entry());
                    }
                    fs::hard_link(&source, &dst).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                }
                // Sparse files still need `tar` to place the holes.
                _ => {
                    entry.set_preserve_permissions(true);
                    entry.set_preserve_mtime(true);
                    let unpacked = entry.unpack_in(root).io_context(|| {
                        format!("extracting {:?} from layer {layer}", path.display())
                    })?;
                    if !unpacked {
                        return Err(unsafe_entry());
                    }
                }
            }
        }
        Ok(())
    }

    /// `.wh..wh..opq` hides everything the lower layers put in this directory.
    fn apply_opaque_whiteout(&mut self, root: &Path, dir: &Path) -> Result<()> {
        // The marker applies to a directory, and only to one inside the
        // rootfs. Reading through a symlink here would clear whatever it points
        // at, which a layer is free to aim anywhere on the host.
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io(format!("inspecting {}", dir.display()), err)),
        }
        // A directory can still sit outside the rootfs when one of its parents
        // is a symlink, so the resolved path has to be checked as well.
        if !self.parents.contains(dir)? {
            return Ok(());
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io(format!("listing {}", dir.display()), err)),
        };
        log!("Whiteout: clearing /{}", relative_display(root, dir));
        for entry in entries {
            let entry = entry.io_context(|| format!("listing {}", dir.display()))?;
            fsutil::remove_any(&entry.path())?;
        }
        self.parents.forget(dir);
        Ok(())
    }

    /// Applies the recorded directory permissions, deepest first.
    pub fn finish(mut self) -> Result<()> {
        self.deferred_modes
            .sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, mode) in &self.deferred_modes {
            // Keep traversal rights: without them cleanup and later runs would fail.
            let mode = mode | 0o700;
            if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(mode))
                && err.kind() != io::ErrorKind::NotFound
            {
                warning!("could not set mode on {}: {err}", path.display());
            }
        }
        Ok(())
    }
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn is_supported(entry_type: EntryType) -> bool {
    matches!(
        entry_type,
        EntryType::Regular
            | EntryType::Directory
            | EntryType::Symlink
            | EntryType::Link
            | EntryType::Continuous
            | EntryType::GNUSparse
    )
}

/// Writes a regular file, replacing `tar`'s own unpacking so that the copy can
/// use a buffer sized for the pipeline rather than std's default. The path and
/// its parent have already been checked and created, so this only has to
/// reproduce the parts of `unpack_in` that a regular file needs: the contents,
/// the mode and the modification time.
///
/// Returns whether something already at `dst` had to be removed first.
fn unpack_regular<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dst: &Path,
    mode: u32,
    buffer: &mut [u8],
) -> io::Result<bool> {
    // Creating with the final mode avoids a window in which the file is more
    // permissive than the layer asked for. Permissions are checked when the
    // file is opened, so a read-only mode does not stop the writes below.
    //
    // The first attempt is exclusive, which costs nothing when the path is
    // free, as it is for every file in the first and largest layer. It also
    // refuses to follow a symlink already sitting there, so the path only has
    // to be cleared on the rare occasion a later layer replaces something,
    // rather than being stat'd and unlinked for every file in the image.
    let mut replaced = false;
    let mut file = match open_exclusive(dst, mode) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            fsutil::remove_any(dst).map_err(io::Error::other)?;
            replaced = true;
            open_exclusive(dst, mode)?
        }
        Err(err) => return Err(err),
    };

    let mut filled = 0;
    loop {
        match entry.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                // Only flush full buffers, so that a stream handing over small
                // reads still turns into large writes.
                if filled == buffer.len() {
                    file.write_all(buffer)?;
                    filled = 0;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    if filled > 0 {
        file.write_all(&buffer[..filled])?;
    }

    // `mode` is what the file was created with, but the umask applies to
    // creation and not to this, so it is still needed to get the mode asked for.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    if let Ok(mtime) = entry.header().mtime() {
        set_mtime(&file, mtime);
    }
    Ok(replaced)
}

fn open_exclusive(dst: &Path, mode: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(dst)
}

/// `tar` reaches for the `filetime` crate to do this; `futimens` on the open
/// file is the same call without the dependency. Timestamps are cosmetic, so a
/// failure is not worth failing the run over.
fn set_mtime(file: &fs::File, mtime: u64) {
    let time = libc::timespec {
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    let _ = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
}

/// The same for a symlink, which has no descriptor to hang the call on and
/// must not be followed to the file it names.
fn set_symlink_mtime(path: &Path, mtime: u64) {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let time = libc::timespec {
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    // SAFETY: the path is a live NUL terminated string and `times` holds the
    // two values utimensat reads.
    let _ = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
}

/// The target of a link entry, or `None` when the header does not name one.
/// An empty target is no target: `symlinkat` would take it and leave a link
/// that resolves to nothing.
fn link_name<R: Read>(entry: &tar::Entry<'_, R>, layer: &str) -> Result<Option<PathBuf>> {
    let target = entry
        .link_name()
        .io_context(|| format!("reading a link name in layer {layer}"))?;
    Ok(target
        .filter(|target| target.as_os_str().as_bytes() != b"")
        .map(|target| target.into_owned()))
}

/// Keeps existing directories (including symlinks to directories) intact so
/// that layouts such as `/lib -> /usr/lib` survive later layers. Returns
/// whether something that was not a directory had to be removed.
fn prepare_directory(dst: &Path) -> Result<bool> {
    match fs::symlink_metadata(dst) {
        Ok(metadata) => {
            let resolves_to_dir = metadata.is_dir()
                || (metadata.file_type().is_symlink()
                    && fs::metadata(dst).map(|m| m.is_dir()).unwrap_or(false));
            if resolves_to_dir {
                return Ok(false);
            }
            fsutil::remove_any(dst)?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::io(format!("inspecting {}", dst.display()), err)),
    }
}

#[derive(Default)]
struct HashState {
    hasher: Sha256,
    bytes: u64,
}

/// Reads the blob and hashes it, handing each buffer to the decompressor by
/// move. With decompression on the critical path, sha256 of the compressed
/// bytes is time the inflater is not inflating, and the read it accompanies is
/// nearly free, so the two belong together on a thread of their own.
///
/// The hash covers exactly the bytes the decompressor is given, so the digest
/// still describes what was extracted rather than a second read of the file.
fn read_and_hash(mut file: fs::File, sender: SyncSender<io::Result<Chunk>>, pool: Pool) -> HashState {
    let mut state = HashState::default();
    loop {
        let mut buf = pool.take();
        match file.read(&mut buf) {
            Ok(0) => return state,
            Ok(read) => {
                state.hasher.update(&buf[..read]);
                state.bytes += read as u64;
                // The consumer has stopped, and has an error of its own to report.
                if sender.send(Ok(Chunk { buf, len: read })).is_err() {
                    return state;
                }
            }
            Err(err) => {
                let _ = sender.send(Err(err));
                return state;
            }
        }
    }
}

/// Runs on the decompression thread: inflates the blob into `sender` and checks
/// it against its descriptor, while a third thread reads and hashes it.
/// Reaching the end of the blob is what makes the digest meaningful, so the
/// size and digest are only reported when the consumer took everything; a
/// consumer that stopped early has an error of its own to report.
fn inflate_blob(
    file: fs::File,
    descriptor: &Descriptor,
    sender: SyncSender<io::Result<Chunk>>,
    pool: Pool,
) -> Result<()> {
    let (raw_sender, raw_receiver) = sync_channel(PIPELINE_DEPTH);
    let (raw_pool, raw_ret) = buffer_pool();
    let hashing = thread::spawn(move || read_and_hash(file, raw_sender, raw_pool));
    let counted = ChunkReader::new(raw_receiver, Some(raw_ret));
    let mut decoder = match decompressor(&descriptor.media_type, counted) {
        Ok(decoder) => decoder,
        Err(err) => {
            let _ = sender.send(Err(io::Error::other(err.to_string())));
            return Err(err);
        }
    };

    loop {
        let mut buf = pool.take();
        let read = match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                let message = err.to_string();
                let _ = sender.send(Err(err));
                return Err(Error::io(
                    format!("reading layer {}", descriptor.digest),
                    io::Error::other(message),
                ));
            }
        };
        if sender.send(Ok(Chunk { buf, len: read })).is_err() {
            return Ok(());
        }
    }
    // The reader thread may still be blocked handing over a buffer, so the
    // decoder, and with it the receiving end, has to go before this joins.
    drop(decoder);
    let state = hashing.join().unwrap_or_default();
    if descriptor.size != 0 && descriptor.size != state.bytes {
        return Err(Error::SizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual: state.bytes,
        });
    }
    let actual = hex_encode(&state.hasher.finalize());
    if actual != parse_digest(&descriptor.digest)?.hex {
        return Err(Error::DigestMismatch {
            digest: descriptor.digest.clone(),
            actual,
        });
    }
    Ok(())
}

/// Runs on the decompression thread when the layer has a checkpoint index:
/// inflates disjoint spans of the blob on every available core and hands them
/// to `sender` in order. Spans need random access, so the whole compressed
/// blob is mapped; hashing it for the digest check then happens over the same
/// pages on a thread of its own, instead of alongside a read.
fn inflate_indexed(
    mut file: fs::File,
    index: &zinfo::Index,
    descriptor: &Descriptor,
    sender: SyncSender<io::Result<Chunk>>,
) -> Result<()> {
    let len = file.metadata().map_or(0, |m| m.len()) as usize;
    let mapped = crate::sys::Mapping::of(&file, len);
    // Nothing to map, or the kernel would not: the spans still need the whole
    // blob, so fall back to a copy of it.
    let mut read = Vec::new();
    if mapped.is_none()
        && let Err(err) = file.read_to_end(&mut read)
    {
        let _ = sender.send(Err(io::Error::other(err.to_string())));
        return Err(Error::io(
            format!("reading layer {}", descriptor.digest),
            err,
        ));
    }
    let blob: &[u8] = match &mapped {
        Some(mapping) => mapping,
        None => &read,
    };

    let spans = index.checkpoints.len();
    let workers = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(spans);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);

    let mut span_error = None;
    let digest = thread::scope(|scope| {
        let hashing = scope.spawn(|| Sha256::digest(&blob));

        // Workers claim span indices; completed spans are put back in order
        // here. The channel bound plus one finished span per worker caps how
        // far decompression runs ahead of the writer.
        let (span_sender, span_receiver) = sync_channel::<(usize, Result<Vec<u8>>)>(workers);
        for _ in 0..workers {
            let span_sender = span_sender.clone();
            let (blob, next, stop) = (&blob, &next, &stop);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= spans {
                        break;
                    }
                    let result = index.extract_span(blob, i);
                    let failed = result.is_err();
                    if span_sender.send((i, result)).is_err() || failed {
                        break;
                    }
                }
            });
        }
        drop(span_sender);

        let mut pending = std::collections::HashMap::new();
        let mut want = 0;
        'reorder: while let Ok((i, result)) = span_receiver.recv() {
            pending.insert(i, result);
            while let Some(result) = pending.remove(&want) {
                want += 1;
                match result {
                    Ok(span) => {
                        let len = span.len();
                        if sender.send(Ok(Chunk { buf: span, len })).is_err() {
                            // The writer stopped; it has an error of its own.
                            stop.store(true, Ordering::Relaxed);
                            break 'reorder;
                        }
                    }
                    Err(err) => {
                        let _ = sender.send(Err(io::Error::other(err.to_string())));
                        span_error = Some(err);
                        stop.store(true, Ordering::Relaxed);
                        break 'reorder;
                    }
                }
            }
        }
        // Dropping the receiver at the end of the scope unblocks any worker
        // still sending, so the implicit joins cannot deadlock.
        hashing.join().unwrap_or_default()
    });

    // The digest verdict comes first: a blob that fails it explains any span
    // error, since the index describes the blob the descriptor names.
    if descriptor.size != 0 && descriptor.size != blob.len() as u64 {
        return Err(Error::SizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual: blob.len() as u64,
        });
    }
    let actual = hex_encode(&digest);
    if actual != parse_digest(&descriptor.digest)?.hex {
        return Err(Error::DigestMismatch {
            digest: descriptor.digest.clone(),
            actual,
        });
    }
    match span_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Presents the inflated chunks as a stream for `tar` to walk, returning each
/// buffer to the producer's pool once it has been drained.
struct ChunkReader {
    chunks: Receiver<io::Result<Chunk>>,
    current: Chunk,
    taken: usize,
    ret: Option<SyncSender<Vec<u8>>>,
}

impl ChunkReader {
    fn new(chunks: Receiver<io::Result<Chunk>>, ret: Option<SyncSender<Vec<u8>>>) -> Self {
        ChunkReader {
            chunks,
            current: Chunk {
                buf: Vec::new(),
                len: 0,
            },
            taken: 0,
            ret,
        }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let available = self.current.len - self.taken;
            if available > 0 {
                let take = available.min(buf.len());
                buf[..take].copy_from_slice(&self.current.buf[self.taken..self.taken + take]);
                self.taken += take;
                return Ok(take);
            }
            // A full pool means the producer has all the buffers it can use,
            // so the spare is dropped rather than blocking the pipeline on it.
            if let Some(ret) = &self.ret
                && !self.current.buf.is_empty()
            {
                let _ = ret.try_send(std::mem::take(&mut self.current.buf));
            }
            match self.chunks.recv() {
                Ok(Ok(chunk)) => {
                    self.current = chunk;
                    self.taken = 0;
                }
                Ok(Err(err)) => return Err(err),
                // The sender is gone, so the blob has been read in full.
                Err(_) => return Ok(0),
            }
        }
    }
}

fn decompressor<'a, R: Read + 'a>(media_type: &str, reader: R) -> Result<Box<dyn Read + 'a>> {
    match compression_of(media_type) {
        Some(Compression::None) => Ok(Box::new(reader)),
        Some(Compression::Gzip) => Ok(Box::new(flate2::read::MultiGzDecoder::new(reader))),
        Some(Compression::Zstd) => {
            let decoder = ruzstd::decoding::StreamingDecoder::new(reader)
                .map_err(|err| Error::io("initialising zstd decoder", io::Error::other(err)))?;
            Ok(Box::new(decoder))
        }
        None => Err(Error::UnsupportedMediaType(media_type.to_string())),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

pub fn compression_of(media_type: &str) -> Option<Compression> {
    // Non-distributable layer types carry the same payload, hence the suffix match.
    match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar"
        | "application/x-tar" => Some(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => Some(Compression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => Some(Compression::Zstd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_layer_media_types_map_to_compression() {
        assert_eq!(
            compression_of("application/vnd.oci.image.layer.v1.tar+gzip"),
            Some(Compression::Gzip)
        );
        assert_eq!(
            compression_of("application/vnd.oci.image.layer.v1.tar+zstd"),
            Some(Compression::Zstd)
        );
        assert_eq!(
            compression_of("application/vnd.oci.image.layer.v1.tar"),
            Some(Compression::None)
        );
        assert_eq!(
            compression_of("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            Some(Compression::Gzip)
        );
        assert_eq!(compression_of("application/vnd.oci.image.config.v1+json"), None);
    }

    #[test]
    fn device_entries_are_not_extracted() {
        assert!(!is_supported(EntryType::Char));
        assert!(!is_supported(EntryType::Block));
        assert!(!is_supported(EntryType::Fifo));
        assert!(is_supported(EntryType::Regular));
        assert!(is_supported(EntryType::Symlink));
        assert!(is_supported(EntryType::Link));
        assert!(is_supported(EntryType::Directory));
    }

    #[test]
    fn the_hashing_thread_sees_every_byte_it_hands_on() {
        let mut blob = scratch("hashing");
        blob.push("blob");
        fs::write(&blob, b"hello").expect("blob");

        let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
        let (pool, ret) = buffer_pool();
        let state = read_and_hash(fs::File::open(&blob).expect("open"), sender, pool);

        let mut passed_on = Vec::new();
        ChunkReader::new(receiver, Some(ret))
            .read_to_end(&mut passed_on)
            .expect("read");
        assert_eq!(passed_on, b"hello");
        assert_eq!(state.bytes, 5);
        assert_eq!(
            hex_encode(&state.hasher.finalize()),
            hex_encode(&Sha256::digest(b"hello"))
        );
    }

    /// A recycled buffer still holds the previous chunk's bytes, so the length
    /// travelling with it is the only thing keeping them out of the stream.
    #[test]
    fn recycled_buffers_do_not_leak_the_previous_chunk() {
        let (sender, receiver) = sync_channel(PIPELINE_DEPTH);
        let (pool, ret) = buffer_pool();

        let mut first = pool.take();
        first[..4].copy_from_slice(b"aaaa");
        sender.send(Ok(Chunk { buf: first, len: 4 })).expect("send");
        let mut second = pool.take();
        second[..2].copy_from_slice(b"bb");
        sender
            .send(Ok(Chunk {
                buf: second,
                len: 2,
            }))
            .expect("send");
        drop(sender);

        let mut streamed = Vec::new();
        ChunkReader::new(receiver, Some(ret))
            .read_to_end(&mut streamed)
            .expect("read");
        assert_eq!(streamed, b"aaaabb");

        // Draining a chunk hands its buffer back with the stale tail intact.
        let recycled = pool.take();
        assert_eq!(&recycled[..4], b"aaaa");
    }

    #[test]
    fn whiteout_names_are_recognised() {
        assert!(OPAQUE_WHITEOUT.starts_with(WHITEOUT_PREFIX));
        assert_eq!(".wh.foo".strip_prefix(WHITEOUT_PREFIX), Some("foo"));
        assert_eq!("foo".strip_prefix(WHITEOUT_PREFIX), None);
    }

    const GZIP_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    const PLAIN_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";

    fn scratch(name: &str) -> Utf8PathBuf {
        let dir = Utf8PathBuf::from(std::env::temp_dir().to_string_lossy().into_owned())
            .join(format!("oci-runtime-extract-{name}-{}", std::process::id()));
        let _ = fsutil::force_remove_dir_all(dir.as_std_path());
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// A tar holding a directory, a file spanning several pipeline chunks, a
    /// small file and a symlink.
    fn sample_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder
            .append_data(&mut header, "dir/", io::empty())
            .expect("dir");

        let large = vec![b'a'; CHUNK_BYTES * 2 + 17];
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(large.len() as u64);
        builder
            .append_data(&mut header, "dir/large", &large[..])
            .expect("large");

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(5);
        builder
            .append_data(&mut header, "dir/small", &b"hello"[..])
            .expect("small");

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder
            .append_link(&mut header, "link", "dir/small")
            .expect("link");

        builder.into_inner().expect("tar")
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).expect("compress");
        encoder.finish().expect("compress")
    }

    /// Writes a blob and returns a descriptor that matches it.
    fn install_blob(root: &Utf8Path, media_type: &str, blob: &[u8]) -> Descriptor {
        let hex = hex_encode(&Sha256::digest(blob));
        let blobs = root.join("blobs").join("sha256");
        fs::create_dir_all(&blobs).expect("blobs");
        fs::write(blobs.join(&hex), blob).expect("blob");
        fs::write(
            root.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .expect("layout");
        fs::write(root.join("index.json"), br#"{"manifests":[]}"#).expect("index");
        Descriptor {
            media_type: media_type.to_string(),
            digest: format!("sha256:{hex}"),
            size: blob.len() as u64,
            platform: None,
        }
    }

    fn extract(root: &Utf8Path, descriptor: &Descriptor) -> Result<Utf8PathBuf> {
        extract_indexed(root, descriptor, None)
    }

    fn extract_indexed(
        root: &Utf8Path,
        descriptor: &Descriptor,
        index_dir: Option<&Utf8Path>,
    ) -> Result<Utf8PathBuf> {
        let layout = Layout::open(root)?;
        let rootfs = root.join("rootfs");
        let mut extractor = RootfsExtractor::new(&rootfs, index_dir)?;
        extractor.apply_layer(&layout, descriptor)?;
        extractor.finish()?;
        Ok(rootfs)
    }

    #[test]
    fn layers_are_unpacked_while_they_are_inflated() {
        for (name, media_type, blob) in [
            ("gzip", GZIP_LAYER, gzip(&sample_tar())),
            ("plain", PLAIN_LAYER, sample_tar()),
        ] {
            let root = scratch(&format!("pipeline-{name}"));
            let descriptor = install_blob(&root, media_type, &blob);
            let rootfs = extract(&root, &descriptor).expect("extract");

            assert!(rootfs.join("dir").is_dir(), "{name}: directory");
            assert_eq!(
                fs::read(rootfs.join("dir/large")).expect("large").len(),
                CHUNK_BYTES * 2 + 17,
                "{name}: file spanning chunk boundaries"
            );
            assert_eq!(
                fs::read_to_string(rootfs.join("dir/small")).expect("small"),
                "hello",
                "{name}: small file"
            );
            assert_eq!(
                fs::read_link(rootfs.join("link")).expect("link"),
                Path::new("dir/small"),
                "{name}: symlink"
            );
            let _ = fsutil::force_remove_dir_all(root.as_std_path());
        }
    }

    /// Symlinks, hard links and directories are placed by this module rather
    /// than by `tar`, so each of their behaviours needs its own cover.
    #[test]
    fn link_entries_are_placed_with_their_metadata() {
        let root = scratch("link-entries");
        let mut builder = tar::Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(5);
        header.set_mtime(1_000_000);
        builder
            .append_data(&mut header, "target", &b"hello"[..])
            .expect("file");

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_mtime(1_234_567);
        builder
            .append_link(&mut header, "sym", "target")
            .expect("symlink");

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        builder
            .append_link(&mut header, "hard", "target")
            .expect("hard link");

        let blob = builder.into_inner().expect("tar");
        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        let rootfs = extract(&root, &descriptor).expect("extract");

        assert_eq!(
            fs::read_link(rootfs.join("sym")).expect("symlink"),
            Path::new("target")
        );
        let metadata = fs::symlink_metadata(rootfs.join("sym")).expect("symlink metadata");
        assert_eq!(
            metadata
                .modified()
                .expect("mtime")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("epoch")
                .as_secs(),
            1_234_567,
            "the symlink itself carries the mtime, not what it points at"
        );
        assert_eq!(
            fs::read_to_string(rootfs.join("hard")).expect("hard link"),
            "hello"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_hard_link_cannot_name_a_target_outside_the_rootfs() {
        let root = scratch("hard-link-escape");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        builder
            .append_link(&mut header, "stolen", "../../etc/passwd")
            .expect("hard link");
        let blob = builder.into_inner().expect("tar");

        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        let err = extract(&root, &descriptor).expect_err("the entry must be refused");
        assert!(
            matches!(err, Error::UnsafeEntry { .. }),
            "expected an unsafe entry, got {err:?}"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_link_entry_without_a_target_is_refused() {
        let root = scratch("empty-link");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_path("sym").expect("path");
        header.set_link_name_literal("").expect("empty link name");
        header.set_cksum();
        builder.append(&header, &[][..]).expect("symlink");
        let blob = builder.into_inner().expect("tar");

        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        let err = extract(&root, &descriptor).expect_err("the entry must be refused");
        assert!(
            matches!(err, Error::UnsafeEntry { .. }),
            "expected an unsafe entry, got {err:?}"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn an_entry_cannot_escape_through_a_symlinked_parent() {        // A layer can ship a symlink out of the rootfs and then an entry
        // underneath it. The parent of that entry does not exist and cannot be
        // resolved, which used to count as safe, so creating it followed the
        // symlink and wrote the file outside the rootfs.
        let root = scratch("escape");
        let outside = scratch("escape-outside");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder
            .append_link(&mut header, "lnk", outside.as_std_path())
            .expect("link");

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(7);
        builder
            .append_data(&mut header, "lnk/sub/file", &b"escaped"[..])
            .expect("file");
        let blob = builder.into_inner().expect("tar");

        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        let err = extract(&root, &descriptor).expect_err("the entry must be refused");
        assert!(
            matches!(err, Error::UnsafeEntry { .. }),
            "expected an unsafe entry, got {err:?}"
        );
        assert!(
            fs::read_dir(outside.as_std_path())
                .expect("outside")
                .next()
                .is_none(),
            "nothing may be written outside the rootfs"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
        let _ = fsutil::force_remove_dir_all(outside.as_std_path());
    }

    #[test]
    fn an_opaque_whiteout_cannot_clear_a_directory_outside_the_rootfs() {
        // The marker names the directory it applies to, and a layer can point
        // that name at a symlink leading out of the rootfs. Reading through it
        // deleted everything at the far end.
        let root = scratch("opaque-escape");
        let outside = scratch("opaque-escape-outside");
        fs::write(outside.join("keep"), b"important").expect("keep");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder
            .append_link(&mut header, "lnk", outside.as_std_path())
            .expect("link");

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(0);
        builder
            .append_data(&mut header, "lnk/.wh..wh..opq", &b""[..])
            .expect("opaque");
        let blob = builder.into_inner().expect("tar");

        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        extract(&root, &descriptor).expect("extract");
        assert_eq!(
            fs::read(outside.join("keep")).expect("keep"),
            b"important",
            "files outside the rootfs may not be removed"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
        let _ = fsutil::force_remove_dir_all(outside.as_std_path());
    }

    #[test]
    fn a_verified_directory_replaced_by_a_symlink_is_checked_again() {
        // Parents are remembered once they have been checked, so a layer that
        // swaps a directory it has already used for a symlink out of the rootfs
        // must drop that memory again, or the entries after it are waved
        // through on the strength of a check that no longer holds.
        let root = scratch("stale");
        let outside = scratch("stale-outside");

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(5);
        builder
            .append_data(&mut header, "a/b/first", &b"first"[..])
            .expect("first");

        // `a/b` is now a checked parent. Replace it with a way out.
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder
            .append_link(&mut header, "a/b", outside.as_std_path())
            .expect("link");

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(6);
        builder
            .append_data(&mut header, "a/b/second", &b"second"[..])
            .expect("second");
        let blob = builder.into_inner().expect("tar");

        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        let err = extract(&root, &descriptor).expect_err("the entry must be refused");
        assert!(
            matches!(err, Error::UnsafeEntry { .. }),
            "expected an unsafe entry, got {err:?}"
        );
        assert!(
            !outside.join("second").exists(),
            "nothing may be written outside the rootfs"
        );

        let _ = fsutil::force_remove_dir_all(root.as_std_path());
        let _ = fsutil::force_remove_dir_all(outside.as_std_path());
    }

    #[test]
    fn file_modes_and_timestamps_survive_extraction() {
        // Unpacking regular files no longer goes through tar, so the parts of
        // its behaviour we still rely on are pinned here. A read-only mode is
        // the interesting case: the file is created with it and written after.
        let mut builder = tar::Builder::new(Vec::new());
        for (name, mode, mtime, contents) in [
            ("readonly", 0o400u32, 1_000_000_000u64, "secret"),
            ("program", 0o755, 1_234_567_890, "#!/bin/sh\n"),
            ("data", 0o644, 7, "plain"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(mode);
            header.set_mtime(mtime);
            header.set_size(contents.len() as u64);
            builder
                .append_data(&mut header, name, contents.as_bytes())
                .expect("entry");
        }
        let blob = builder.into_inner().expect("tar");

        let root = scratch("modes");
        let descriptor = install_blob(&root, PLAIN_LAYER, &blob);
        let rootfs = extract(&root, &descriptor).expect("extract");

        for (name, mode, mtime, contents) in [
            ("readonly", 0o400u32, 1_000_000_000u64, "secret"),
            ("program", 0o755, 1_234_567_890, "#!/bin/sh\n"),
            ("data", 0o644, 7, "plain"),
        ] {
            let path = rootfs.join(name);
            assert_eq!(
                fs::read_to_string(&path).expect(name),
                contents,
                "{name}: contents"
            );
            let metadata = fs::metadata(&path).expect(name);
            assert_eq!(metadata.permissions().mode() & 0o7777, mode, "{name}: mode");
            let modified = metadata
                .modified()
                .expect(name)
                .duration_since(std::time::UNIX_EPOCH)
                .expect(name)
                .as_secs();
            assert_eq!(modified, mtime, "{name}: mtime");
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn padding_after_the_tar_marker_is_still_verified() {
        // More padding than the pipeline can hold, so the blob is only read to
        // the end, and therefore only checked, because unpacking drains it.
        let mut tar = sample_tar();
        tar.extend_from_slice(&vec![0u8; PIPELINE_DEPTH * CHUNK_BYTES * 2]);
        let blob = gzip(&tar);

        let root = scratch("drain-ok");
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);
        extract(&root, &descriptor).expect("a padded blob extracts");
        let _ = fsutil::force_remove_dir_all(root.as_std_path());

        let root = scratch("drain-short");
        let mut descriptor = install_blob(&root, GZIP_LAYER, &blob);
        descriptor.size -= 1;
        match extract(&root, &descriptor) {
            Err(Error::SizeMismatch { .. }) => {}
            other => panic!("expected a size mismatch, got {other:?}"),
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_blob_that_does_not_match_its_digest_is_rejected() {
        let root = scratch("digest");
        let mut descriptor = install_blob(&root, GZIP_LAYER, &gzip(&sample_tar()));

        // Leave the blob where the descriptor says it is, but change what is in it.
        let mut altered = sample_tar();
        altered.extend_from_slice(&[0u8; 1024]);
        let altered = gzip(&altered);
        let path = Layout::open(&root)
            .expect("layout")
            .blob_path(&descriptor.digest)
            .expect("blob path");
        fs::write(&path, &altered).expect("blob");
        descriptor.size = altered.len() as u64;

        match extract(&root, &descriptor) {
            Err(Error::DigestMismatch { .. }) => {}
            other => panic!("expected a digest mismatch, got {other:?}"),
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_blob_that_does_not_match_its_size_is_rejected() {
        let root = scratch("size");
        let mut descriptor = install_blob(&root, GZIP_LAYER, &gzip(&sample_tar()));
        descriptor.size += 1;
        match extract(&root, &descriptor) {
            Err(Error::SizeMismatch { .. }) => {}
            other => panic!("expected a size mismatch, got {other:?}"),
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_corrupt_blob_is_reported_rather_than_hanging() {
        let root = scratch("corrupt");
        let mut blob = gzip(&sample_tar());
        let tail = blob.len() - 64;
        blob[tail..].fill(0xff);
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);
        assert!(extract(&root, &descriptor).is_err());
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn an_unsupported_media_type_is_reported() {
        let root = scratch("media-type");
        let descriptor = install_blob(&root, "application/x-nonsense", &sample_tar());
        match extract(&root, &descriptor) {
            Err(Error::UnsupportedMediaType(_)) => {}
            other => panic!("expected an unsupported media type, got {other:?}"),
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    /// Compressible but non-repeating, so deflate emits many dynamic blocks
    /// and a small span yields plenty of checkpoints.
    fn random_bytes(len: usize) -> Vec<u8> {
        let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
        let mut out = Vec::with_capacity(len + 16);
        let mut state: u64 = 0x2545F4914F6CDD1D;
        while out.len() < len {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            out.extend_from_slice(words[(state >> 33) as usize % words.len()].as_bytes());
            out.extend_from_slice(state.to_le_bytes()[..3].as_ref());
        }
        out.truncate(len);
        out
    }

    /// A tar large enough for several spans: one incompressible file plus the
    /// small entries the pipeline tests use.
    fn multi_span_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let random = random_bytes(1 << 20);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(random.len() as u64);
        builder
            .append_data(&mut header, "random", &random[..])
            .expect("random");

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(5);
        builder
            .append_data(&mut header, "small", &b"hello"[..])
            .expect("small");
        builder.into_inner().expect("tar")
    }

    /// Compresses at the default level; `gzip` above uses the fast level,
    /// which emits too few deflate block boundaries to checkpoint.
    fn gzip_default(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).expect("compress");
        encoder.finish().expect("compress")
    }

    /// Builds a checkpoint index for the blob and installs it where the
    /// extractor looks, returning the index directory.
    fn install_index(root: &Utf8Path, descriptor: &Descriptor, blob: &[u8]) -> Utf8PathBuf {
        let index = zinfo::Index::build(blob, 64 * 1024).expect("index");
        assert!(
            index.checkpoints.len() > 2,
            "the sample must produce several checkpoints, got {}",
            index.checkpoints.len()
        );
        let dir = root.join("indexes");
        fs::create_dir_all(&dir).expect("index dir");
        let hex = parse_digest(&descriptor.digest).expect("digest").hex;
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).expect("serialise");
        fs::write(dir.join(format!("{hex}.zinfo")), bytes).expect("install index");
        dir
    }

    #[test]
    fn an_indexed_layer_extracts_the_same_bytes() {
        let root = scratch("indexed");
        let tar = multi_span_tar();
        let blob = gzip_default(&tar);
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);
        let dir = install_index(&root, &descriptor, &blob);

        let rootfs = extract_indexed(&root, &descriptor, Some(&dir)).expect("extract");
        assert_eq!(
            fs::read(rootfs.join("random")).expect("random"),
            random_bytes(1 << 20),
            "indexed extraction must reproduce the exact bytes"
        );
        assert_eq!(
            fs::read_to_string(rootfs.join("small")).expect("small"),
            "hello"
        );
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_layer_without_an_index_file_streams_as_before() {
        let root = scratch("indexed-absent");
        let blob = gzip_default(&multi_span_tar());
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);
        let dir = root.join("indexes");
        fs::create_dir_all(&dir).expect("empty index dir");

        let rootfs = extract_indexed(&root, &descriptor, Some(&dir)).expect("extract");
        assert_eq!(
            fs::read_to_string(rootfs.join("small")).expect("small"),
            "hello"
        );
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_tampered_index_fails_the_extraction() {
        let root = scratch("indexed-tampered");
        let blob = gzip_default(&multi_span_tar());
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);
        let dir = install_index(&root, &descriptor, &blob);

        // Flip the first checkpoint's span CRC (after magic, length and count).
        let hex = parse_digest(&descriptor.digest).expect("digest").hex;
        let path = dir.join(format!("{hex}.zinfo"));
        let mut bytes = fs::read(&path).expect("index");
        bytes[32] ^= 0xff;
        fs::write(&path, bytes).expect("index");

        let err = extract_indexed(&root, &descriptor, Some(&dir))
            .expect_err("a corrupt index must not extract");
        assert!(
            err.to_string().contains("span checksum"),
            "expected a span checksum failure, got {err}"
        );
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn an_index_for_another_blob_is_an_error_not_a_panic() {
        let root = scratch("indexed-stale");
        let blob = gzip_default(&multi_span_tar());
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);

        // An index built from a different, shorter blob: checkpoints point
        // into compressed bytes that do not exist.
        let other = gzip_default(&random_bytes(512 * 1024));
        let index = zinfo::Index::build(&other, 64 * 1024).expect("index");
        let dir = root.join("indexes");
        fs::create_dir_all(&dir).expect("index dir");
        let hex = parse_digest(&descriptor.digest).expect("digest").hex;
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).expect("serialise");
        fs::write(dir.join(format!("{hex}.zinfo")), bytes).expect("install index");

        assert!(extract_indexed(&root, &descriptor, Some(&dir)).is_err());
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }

    #[test]
    fn a_digest_mismatch_wins_over_index_errors() {
        let root = scratch("indexed-digest");
        let blob = gzip_default(&multi_span_tar());
        let descriptor = install_blob(&root, GZIP_LAYER, &blob);
        let dir = install_index(&root, &descriptor, &blob);

        // Replace the blob: the index no longer matches, but the reason is
        // that the blob is not what the descriptor promised.
        let altered = gzip_default(&random_bytes(1 << 20));
        let path = Layout::open(&root)
            .expect("layout")
            .blob_path(&descriptor.digest)
            .expect("blob path");
        fs::write(&path, &altered).expect("blob");
        let mut descriptor = descriptor;
        descriptor.size = altered.len() as u64;

        match extract_indexed(&root, &descriptor, Some(&dir)) {
            Err(Error::DigestMismatch { .. }) => {}
            other => panic!("expected a digest mismatch, got {other:?}"),
        }
        let _ = fsutil::force_remove_dir_all(root.as_std_path());
    }
}
