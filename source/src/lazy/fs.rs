//! The filesystem the container sees.
//!
//! Metadata is answered from the tree, which is the whole of the image's
//! namespace and needs no blob to build. A regular file's bytes are fetched
//! the first time something opens it, written into a backing file named after
//! its inode, and every read and write after that goes to the backing file. A
//! rootfs read right through therefore ends up as the tree extraction would
//! have written, only paid for a file at a time and only for the files that
//! are used.
//!
//! The backing directory is flat and keyed by inode, so renaming, unlinking
//! and hard linking are changes to the tree alone: nothing on disk has to
//! move, and two names for one file are two names for one backing file.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};

use super::source::{Scratch, Source};
use super::tree::{Bodies, Body, Content, Kind, Node, ROOT, Special, Tree};
use crate::error::IoContext;

/// How long the kernel may trust what it was told. Everything that changes the
/// tree comes through here, so a longer life would only risk a stale answer to
/// something changed behind the kernel's back, which nothing does.
const TTL: Duration = Duration::from_secs(1);

/// Inode numbers are never reused, so nothing needs a generation to tell two
/// files with the same number apart.
const GENERATION: Generation = Generation(0);

const BLOCK_SIZE: u32 = 4096;

/// Fetching a file takes the lock its inode falls on rather than one for the
/// whole filesystem, so two threads fetching two files do not queue.
const SHARDS: usize = 64;

thread_local! {
    /// The buffer and decoders a thread reuses from body to body. The session
    /// runs a fixed set of threads, so this is a fixed cost.
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

pub struct Rootfs {
    tree: RwLock<Tree>,
    source: Source,
    bodies: Bodies,
    layers: usize,
    backing: PathBuf,
    fetching: Vec<Mutex<()>>,
    /// Who the container sees as the owner of everything in the image. The
    /// caller is mapped to root inside the container, so this is root there,
    /// which is what extraction leaves behind as well.
    uid: u32,
    gid: u32,
    handles: Mutex<HashMap<u64, Arc<File>>>,
    next_handle: AtomicU64,
}

impl Rootfs {
    pub fn new(
        tree: Tree,
        bodies: Bodies,
        source: Source,
        layers: usize,
        backing: PathBuf,
        uid: u32,
        gid: u32,
    ) -> Rootfs {
        Rootfs {
            tree: RwLock::new(tree),
            source,
            bodies,
            layers,
            backing,
            fetching: (0..SHARDS).map(|_| Mutex::new(())).collect(),
            uid,
            gid,
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, Tree>, Errno> {
        self.tree.read().map_err(|_| Errno::EIO)
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Tree>, Errno> {
        self.tree.write().map_err(|_| Errno::EIO)
    }

    fn backing_path(&self, ino: u64) -> PathBuf {
        self.backing.join(ino.to_string())
    }

    /// The bytes still owed to a file, or `None` when it is already backed.
    fn owed(&self, ino: u64) -> Result<Option<(Body, u64)>, Errno> {
        let tree = self.read()?;
        let node = tree.get(ino).ok_or(Errno::ENOENT)?;
        match &node.kind {
            Kind::File(Content::Layer(body)) => Ok(Some((*body, node.mtime))),
            Kind::File(Content::Backed) => Ok(None),
            Kind::Directory { .. } => Err(Errno::EISDIR),
            _ => Err(Errno::EINVAL),
        }
    }

    /// Fetches a file's bytes if this is the first time anything asked for
    /// them, and with them the rest of the span they are in.
    ///
    /// The span is what inflating reaches whatever was asked for, so placing
    /// only the file that was asked for would inflate the same span again for
    /// the next one. Reading a rootfs right through costs what extracting it
    /// costs this way, rather than a span per file.
    ///
    /// The tree lock is not held while a span is inflated. That is the one
    /// slow thing here, and holding it would stop every other request for as
    /// long as it took.
    fn fetch(&self, ino: u64) -> Result<(), Errno> {
        let Some((body, _)) = self.owed(ino)? else {
            return Ok(());
        };
        let claim = self.source.span_of(body) * self.layers + body.layer as usize;
        // The lock guards nothing of its own, so a worker that panicked while
        // holding it leaves the rest of the session usable.
        let _claim = self.fetching[claim % SHARDS]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Whoever held the claim was inflating this very span.
        let Some((body, _)) = self.owed(ino)? else {
            return Ok(());
        };

        SCRATCH
            .with(|scratch| self.fetch_span(body, &mut scratch.borrow_mut()))
            .map_err(|err| {
                crate::log::warn(format!("could not fetch an image file: {err}"));
                Errno::EIO
            })
    }

    fn fetch_span(&self, body: Body, scratch: &mut Scratch) -> crate::error::Result<()> {
        let window = self.source.inflate(body, scratch)?;
        let owed: Vec<(u64, Body, u64)> = {
            let tree = self.read().map_err(io_error)?;
            self.bodies
                .within(body.layer, window.base..window.end)
                .iter()
                .filter_map(|&(_, ino)| match tree.get(ino)?.kind {
                    Kind::File(Content::Layer(body)) => Some((ino, body, tree.get(ino)?.mtime)),
                    _ => None,
                })
                .collect()
        };

        for (ino, body, mtime) in owed {
            // A body running past the end of the window belongs to the fetch
            // that starts on it, which inflates far enough to hold it.
            let Ok(bytes) = self.source.bytes(body, &window, scratch) else {
                continue;
            };
            let staged = self.backing.join(format!(
                "{ino}.{}.part",
                self.next_handle.fetch_add(1, Ordering::Relaxed)
            ));
            Source::place(&staged, bytes, mtime)?;
            self.commit(ino, &staged)?;
        }
        Ok(())
    }

    /// Puts a staged body in place, unless the file stopped being the image's
    /// while it was being written. Both halves happen under the tree lock, so
    /// nothing can come between them and lose what the container wrote.
    fn commit(&self, ino: u64, staged: &std::path::Path) -> crate::error::Result<()> {
        let mut tree = self.write().map_err(io_error)?;
        match tree.get_mut(ino).map(|node| &mut node.kind) {
            Some(Kind::File(content @ Content::Layer(_))) => {
                fs::rename(staged, self.backing_path(ino))
                    .io_context(|| format!("placing {}", self.backing_path(ino).display()))?;
                *content = Content::Backed;
            }
            _ => {
                let _ = fs::remove_file(staged);
            }
        }
        Ok(())
    }

    fn open_backing(&self, ino: u64) -> Result<Arc<File>, Errno> {
        self.fetch(ino)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.backing_path(ino))?;
        Ok(Arc::new(file))
    }

    fn hold(&self, file: Arc<File>) -> FileHandle {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(handle, file);
        FileHandle(handle)
    }

    fn held(&self, handle: FileHandle) -> Result<Arc<File>, Errno> {
        self.handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&handle.0)
            .cloned()
            .ok_or(Errno::EBADF)
    }

    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let (kind, size, mtime) = match &node.kind {
            Kind::Directory { .. } => (FileType::Directory, BLOCK_SIZE as u64, node.mtime),
            Kind::Symlink(target) => (FileType::Symlink, target.len() as u64, node.mtime),
            Kind::File(Content::Layer(body)) => (FileType::RegularFile, body.size, node.mtime),
            // The container may have changed it since it was fetched, and the
            // backing file is the only place that would show.
            Kind::File(Content::Backed) => match fs::metadata(self.backing_path(ino)) {
                Ok(metadata) => (
                    FileType::RegularFile,
                    metadata.len(),
                    metadata.mtime().max(0) as u64,
                ),
                Err(_) => (FileType::RegularFile, 0, node.mtime),
            },
            Kind::Special(Special::Fifo) => (FileType::NamedPipe, 0, node.mtime),
            Kind::Special(Special::Socket) => (FileType::Socket, 0, node.mtime),
            Kind::Gone => (FileType::RegularFile, 0, node.mtime),
        };
        let time = UNIX_EPOCH + Duration::from_secs(mtime);
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind,
            perm: (node.mode & 0o7777) as u16,
            nlink: node.nlink.max(1),
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn attr_of(&self, ino: u64) -> Result<FileAttr, Errno> {
        let tree = self.read()?;
        let node = tree.get(ino).ok_or(Errno::ENOENT)?;
        Ok(self.attr(ino, node))
    }

    /// Adds a freshly made node under `parent`, refusing a name already taken.
    fn make(&self, parent: u64, name: &OsStr, node: Node) -> Result<(u64, FileAttr), Errno> {
        let mut tree = self.write()?;
        if !tree.get(parent).ok_or(Errno::ENOENT)?.is_directory() {
            return Err(Errno::ENOTDIR);
        }
        if tree.lookup(parent, name.as_bytes()).is_some() {
            return Err(Errno::EEXIST);
        }
        let ino = tree.push(node);
        tree.link(parent, name.as_bytes().to_vec(), ino)
            .ok_or(Errno::EIO)?;
        let attr = self.attr(ino, tree.get(ino).ok_or(Errno::EIO)?);
        Ok((ino, attr))
    }

    /// Takes a name away, and with it the backing file when nothing else names
    /// what it held.
    fn remove(&self, parent: u64, name: &OsStr, directory: bool) -> Result<(), Errno> {
        let mut tree = self.write()?;
        let ino = tree.lookup(parent, name.as_bytes()).ok_or(Errno::ENOENT)?;
        let node = tree.get(ino).ok_or(Errno::ENOENT)?;
        match (directory, node.is_directory()) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            _ => {}
        }
        if directory && !node.children().is_some_and(|children| children.is_empty()) {
            return Err(Errno::ENOTEMPTY);
        }
        let (ino, last) = tree.unlink(parent, name.as_bytes()).ok_or(Errno::ENOENT)?;
        drop(tree);
        if last {
            // Anything still holding it open keeps its own descriptor, exactly
            // as it would for a file unlinked on any other filesystem.
            let _ = fs::remove_file(self.backing_path(ino));
        }
        Ok(())
    }

    /// True when `ino` is `under` or one of its ancestors, which is what a
    /// rename must not do to a directory.
    fn within(&self, tree: &Tree, ino: u64, under: u64) -> bool {
        let mut at = under;
        loop {
            if at == ino {
                return true;
            }
            match tree.get(at).and_then(Node::parent) {
                Some(parent) if parent != at => at = parent,
                _ => return false,
            }
        }
    }
}

/// Answers a request, turning the one error type used here into a reply.
macro_rules! answer {
    ($reply:expr, $body:expr) => {
        match $body {
            Ok(value) => value,
            Err(err) => return $reply.error(err),
        }
    };
}

impl Filesystem for Rootfs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let ino = answer!(reply, {
            self.read()
                .and_then(|tree| tree.lookup(parent.0, name.as_bytes()).ok_or(Errno::ENOENT))
        });
        let attr = answer!(reply, self.attr_of(ino));
        reply.entry(&TTL, &attr, GENERATION);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let attr = answer!(reply, self.attr_of(ino.0));
        reply.attr(&TTL, &attr);
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Ownership is not the image's to give: extraction leaves everything
        // owned by whoever ran it, and so does this.
        if let Some(size) = size {
            answer!(reply, self.fetch(ino.0));
            let file = answer!(
                reply,
                fs::OpenOptions::new()
                    .write(true)
                    .open(self.backing_path(ino.0))
                    .map_err(Errno::from)
            );
            answer!(reply, file.set_len(size).map_err(Errno::from));
        }
        if let Some(mode) = mode {
            let mut tree = answer!(reply, self.write());
            let node = answer!(reply, tree.get_mut(ino.0).ok_or(Errno::ENOENT));
            node.mode = mode & 0o7777;
        }
        // Only the mtime is kept: everything here reports one time for all
        // three, as extraction leaves behind.
        if let Some(mtime) = mtime {
            let when = match mtime {
                TimeOrNow::SpecificTime(when) => when
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs()),
                TimeOrNow::Now => now(),
            };
            let mut tree = answer!(reply, self.write());
            answer!(reply, tree.get_mut(ino.0).ok_or(Errno::ENOENT)).mtime = when;
            drop(tree);
            touch(&self.backing_path(ino.0), when);
        }
        let attr = answer!(reply, self.attr_of(ino.0));
        reply.attr(&TTL, &attr);
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let tree = answer!(reply, self.read());
        let node = answer!(reply, tree.get(ino.0).ok_or(Errno::ENOENT));
        match &node.kind {
            Kind::Symlink(target) => reply.data(target),
            _ => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let file = answer!(reply, self.open_backing(ino.0));
        reply.opened(self.hold(file), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let file = answer!(reply, self.held(fh));
        let mut buffer = vec![0; size as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read_at(&mut buffer[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return reply.error(err.into()),
            }
        }
        reply.data(&buffer[..filled]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let file = answer!(reply, self.held(fh));
        let mut written = 0;
        while written < data.len() {
            match file.write_at(&data[written..], offset + written as u64) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return reply.error(err.into()),
            }
        }
        reply.written(written as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let file = answer!(reply, self.held(fh));
        let synced = if datasync {
            file.sync_data()
        } else {
            file.sync_all()
        };
        match synced {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&fh.0);
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let tree = answer!(reply, self.read());
        let node = answer!(reply, tree.get(ino.0).ok_or(Errno::ENOENT));
        let children = answer!(reply, node.children().ok_or(Errno::ENOTDIR));
        let parent = node.parent().unwrap_or(ROOT);

        let here = [
            (ino.0, FileType::Directory, b".".as_slice()),
            (parent, FileType::Directory, b"..".as_slice()),
        ];
        let entries = here.into_iter().chain(children.iter().map(|(name, &ino)| {
            let kind = tree
                .get(ino)
                .map_or(FileType::RegularFile, |node| kind_of(node));
            (ino, kind, name.as_slice())
        }));
        for (at, (ino, kind, name)) in entries.enumerate().skip(offset as usize) {
            // The offset the kernel comes back with is where to carry on from,
            // so it is one past what has just been added.
            if reply.add(INodeNo(ino), at as u64 + 1, kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let node = Node::directory(mode & !umask & 0o7777);
        let (_, attr) = answer!(reply, self.make(parent.0, name, node));
        reply.entry(&TTL, &attr, GENERATION);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let node = Node::file(Content::Backed, mode & !umask & 0o7777, now());
        let (ino, attr) = answer!(reply, self.make(parent.0, name, node));
        let file = answer!(reply, {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(self.backing_path(ino))
                .map_err(Errno::from)
        });
        reply.created(
            &TTL,
            &attr,
            GENERATION,
            self.hold(Arc::new(file)),
            FopenFlags::empty(),
        );
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let permissions = mode & !umask & 0o7777;
        let node = match mode & libc::S_IFMT {
            libc::S_IFIFO => Node::special(Special::Fifo, permissions, now()),
            libc::S_IFSOCK => Node::special(Special::Socket, permissions, now()),
            libc::S_IFREG | 0 => Node::file(Content::Backed, permissions, now()),
            // Device nodes need a privilege the launcher does not have, and an
            // image's own are skipped by every route.
            _ => return reply.error(Errno::EPERM),
        };
        let (ino, attr) = answer!(reply, self.make(parent.0, name, node));
        if matches!(attr.kind, FileType::RegularFile)
            && let Err(err) = fs::File::create(self.backing_path(ino))
        {
            return reply.error(err.into());
        }
        reply.entry(&TTL, &attr, GENERATION);
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let node = Node::symlink(target.as_os_str().as_bytes().to_vec(), now());
        let (_, attr) = answer!(reply, self.make(parent.0, link_name, node));
        reply.entry(&TTL, &attr, GENERATION);
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let mut tree = answer!(reply, self.write());
        if answer!(reply, tree.get(ino.0).ok_or(Errno::ENOENT)).is_directory() {
            return reply.error(Errno::EPERM);
        }
        if tree.lookup(newparent.0, newname.as_bytes()).is_some() {
            return reply.error(Errno::EEXIST);
        }
        answer!(
            reply,
            tree.link(newparent.0, newname.as_bytes().to_vec(), ino.0)
                .ok_or(Errno::ENOENT)
        );
        let attr = self.attr(ino.0, answer!(reply, tree.get(ino.0).ok_or(Errno::EIO)));
        reply.entry(&TTL, &attr, GENERATION);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        answer!(reply, self.remove(parent.0, name, false));
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        answer!(reply, self.remove(parent.0, name, true));
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::ENOSYS);
        }
        let mut tree = answer!(reply, self.write());
        let ino = answer!(
            reply,
            tree.lookup(parent.0, name.as_bytes()).ok_or(Errno::ENOENT)
        );
        // Moving a directory under itself would cut it out of the tree.
        if self.within(&tree, ino, newparent.0) {
            return reply.error(Errno::EINVAL);
        }

        let mut orphaned = None;
        if let Some(standing) = tree.lookup(newparent.0, newname.as_bytes()) {
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                return reply.error(Errno::EEXIST);
            }
            let moving = answer!(reply, tree.get(ino).ok_or(Errno::ENOENT)).is_directory();
            let standing_node = answer!(reply, tree.get(standing).ok_or(Errno::ENOENT));
            match (moving, standing_node.is_directory()) {
                (true, false) => return reply.error(Errno::ENOTDIR),
                (false, true) => return reply.error(Errno::EISDIR),
                (true, true) if !standing_node.children().is_some_and(|c| c.is_empty()) => {
                    return reply.error(Errno::ENOTEMPTY);
                }
                _ => {}
            }
            let (gone, last) = answer!(
                reply,
                tree.unlink(newparent.0, newname.as_bytes())
                    .ok_or(Errno::ENOENT)
            );
            orphaned = last.then_some(gone);
        }

        answer!(
            reply,
            tree.rename(parent.0, name.as_bytes(), newparent.0, newname.as_bytes())
                .ok_or(Errno::EIO)
        );
        drop(tree);
        if let Some(gone) = orphaned {
            let _ = fs::remove_file(self.backing_path(gone));
        }
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // The tree is memory, so there is nothing behind it to flush.
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let files = self.read().map_or(0, |tree| tree.len() as u64);
        // Writes land in the backing directory, so what it sits on is the
        // space the container actually has.
        let space = space_at(&self.backing);
        reply.statfs(
            space.blocks,
            space.free,
            space.free,
            files,
            space.free,
            space.block_size,
            255,
            space.block_size,
        );
    }
}

struct Space {
    blocks: u64,
    free: u64,
    block_size: u32,
}

fn space_at(path: &std::path::Path) -> Space {
    let mut stat = unsafe { std::mem::zeroed::<libc::statvfs>() };
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return Space {
            blocks: 0,
            free: 0,
            block_size: BLOCK_SIZE,
        };
    };
    // SAFETY: the path is a live NUL terminated string and `stat` is the
    // struct statvfs fills.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Space {
            blocks: 0,
            free: 0,
            block_size: BLOCK_SIZE,
        };
    }
    Space {
        blocks: stat.f_blocks,
        free: stat.f_bavail,
        block_size: stat.f_frsize.max(1) as u32,
    }
}

/// Timestamps are cosmetic, so a failure is not worth failing a request over.
/// A file still only in its layer has none of its own yet, and takes the one
/// the tree holds when it is fetched.
fn touch(path: &std::path::Path, mtime: u64) {
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

fn kind_of(node: &Node) -> FileType {
    match &node.kind {
        Kind::Directory { .. } => FileType::Directory,
        Kind::Symlink(_) => FileType::Symlink,
        Kind::Special(Special::Fifo) => FileType::NamedPipe,
        Kind::Special(Special::Socket) => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A poisoned lock means a worker panicked, which is not something a request
/// can recover from.
fn io_error(_: Errno) -> crate::error::Error {
    crate::error::Error::io(
        "serving the root filesystem",
        std::io::Error::other("the tree is no longer usable"),
    )
}
