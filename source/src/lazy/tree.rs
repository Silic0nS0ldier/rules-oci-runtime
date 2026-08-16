//! The tree a resolved image leaves behind, as inodes.
//!
//! The plan already says what survives the layers: the directories, the files
//! and where their bodies sit, the symlinks and the hard links. That is the
//! whole namespace, so it can be built without touching a blob and answered
//! out of memory, with only the bodies fetched when something asks for them.
//!
//! Inode numbers are indices into one vector and are never reused. A number
//! the kernel still holds therefore always names the file it was given, which
//! is what lets `forget` do nothing.

use std::collections::{BTreeMap, HashMap};

use crate::entries::Table;
use crate::extract::Work;

/// FUSE reserves the first inode for the root of a filesystem.
pub const ROOT: u64 = 1;

/// What `create_dir` would have given a directory nothing names, which is what
/// the plan assumes for the parents it invents.
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;

/// The path the tables give the rootfs itself.
const ROOT_ENTRY: &[u8] = b".";

/// Where a file's bytes sit in the uncompressed stream of its layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Body {
    pub layer: u32,
    pub offset: u64,
    pub size: u64,
}

/// Where a regular file's contents currently are.
#[derive(Debug, PartialEq, Eq)]
pub enum Content {
    /// Still only in the layer.
    Layer(Body),
    /// Written out under the backing directory, which is the content from then
    /// on: the container may have changed it since.
    Backed,
}

#[derive(Debug)]
pub enum Kind {
    Directory {
        parent: u64,
        children: BTreeMap<Vec<u8>, u64>,
    },
    File(Content),
    Symlink(Vec<u8>),
    /// A fifo or a socket the container made. Once the inode exists the kernel
    /// serves it without asking, so there is nothing behind it.
    Special(Special),
    /// Unlinked. The number stays taken, so a handle the kernel still holds
    /// cannot come to name something else.
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    Fifo,
    Socket,
}

#[derive(Debug)]
pub struct Node {
    pub kind: Kind,
    pub mode: u32,
    pub mtime: u64,
    /// Names pointing at this inode. A directory counts `.` and its entry in
    /// its parent, and gains one more for each subdirectory's `..`.
    pub nlink: u32,
}

impl Node {
    pub fn directory(mode: u32) -> Node {
        Node {
            kind: Kind::Directory {
                parent: ROOT,
                children: BTreeMap::new(),
            },
            mode,
            mtime: 0,
            nlink: 2,
        }
    }

    pub fn file(content: Content, mode: u32, mtime: u64) -> Node {
        Node {
            kind: Kind::File(content),
            mode,
            mtime,
            nlink: 0,
        }
    }

    pub fn symlink(target: Vec<u8>, mtime: u64) -> Node {
        Node {
            kind: Kind::Symlink(target),
            mode: 0o777,
            mtime,
            nlink: 0,
        }
    }

    pub fn special(special: Special, mode: u32, mtime: u64) -> Node {
        Node {
            kind: Kind::Special(special),
            mode,
            mtime,
            nlink: 0,
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(self.kind, Kind::Directory { .. })
    }

    pub fn children(&self) -> Option<&BTreeMap<Vec<u8>, u64>> {
        match &self.kind {
            Kind::Directory { children, .. } => Some(children),
            _ => None,
        }
    }

    pub fn parent(&self) -> Option<u64> {
        match &self.kind {
            Kind::Directory { parent, .. } => Some(*parent),
            _ => None,
        }
    }
}

pub struct Tree {
    nodes: Vec<Node>,
}

/// Where each layer's files sit in its uncompressed stream, in that order.
///
/// Inflating reaches a whole span at a time whatever was asked for, so a fetch
/// that placed only the file it was asked for would inflate the same span
/// again for the next one. This is what the rest of the span holds.
#[derive(Default)]
pub struct Bodies {
    by_layer: Vec<Vec<(u64, u64)>>,
}

impl Bodies {
    /// The files of `layer` whose bodies begin in `window`, in stream order.
    pub fn within(&self, layer: u32, window: std::ops::Range<u64>) -> &[(u64, u64)] {
        let Some(bodies) = self.by_layer.get(layer as usize) else {
            return &[];
        };
        let from = bodies.partition_point(|(offset, _)| *offset < window.start);
        let to = bodies.partition_point(|(offset, _)| *offset < window.end);
        &bodies[from..to]
    }
}

impl Tree {
    /// The namespace the image ends up with, or `None` when the plan describes
    /// one that cannot be built: a hard link naming nothing, or an entry whose
    /// parent directory the plan never claimed.
    ///
    /// Either means this and the plan disagree about the image, and an image
    /// served differently from the way it extracts is worse than one that
    /// takes the slow route.
    pub fn build(
        directories: &[(Vec<u8>, u32)],
        tables: &[Table],
        work: &Work,
    ) -> Option<(Tree, Bodies)> {
        let mut tree = Tree {
            nodes: vec![Node::directory(DEFAULT_DIRECTORY_MODE)],
        };
        let mut bodies = Bodies {
            by_layer: vec![Vec::new(); work.files.len()],
        };
        let mut by_path: HashMap<Vec<u8>, u64> = HashMap::new();
        by_path.insert(ROOT_ENTRY.to_vec(), ROOT);

        // Parents before children, which is the order the plan hands them over.
        for (path, mode) in directories {
            if path.as_slice() == ROOT_ENTRY {
                tree.nodes[0].mode = *mode;
                continue;
            }
            let ino = tree.push(Node::directory(*mode));
            tree.attach(&mut by_path, path, ino)?;
        }

        for (layer, entries) in work.files.iter().enumerate() {
            let table = tables.get(layer)?;
            for &entry in entries {
                let entry = table.entries.get(entry as usize)?;
                let body = Body {
                    layer: layer as u32,
                    offset: entry.offset,
                    size: entry.size,
                };
                let ino = tree.push(Node::file(Content::Layer(body), entry.mode, entry.mtime));
                tree.attach(&mut by_path, &entry.path, ino)?;
                bodies.by_layer[layer].push((entry.offset, ino));
            }
            // The plan orders them by offset already; a table that did not
            // would leave the search below answering nonsense.
            bodies.by_layer[layer].sort_unstable();
        }

        for &(layer, entry) in &work.symlinks {
            let entry = tables.get(layer as usize)?.entries.get(entry as usize)?;
            let ino = tree.push(Node::symlink(entry.link.clone(), entry.mtime));
            tree.attach(&mut by_path, &entry.path, ino)?;
        }

        // Hard links come last, when every file they can name has been placed.
        for &(layer, entry) in &work.hard_links {
            let entry = tables.get(layer as usize)?.entries.get(entry as usize)?;
            let target = *by_path.get(entry.link.as_slice())?;
            tree.attach(&mut by_path, &entry.path, target)?;
        }

        Some((tree, bodies))
    }

    pub fn get(&self, ino: u64) -> Option<&Node> {
        let node = self.nodes.get(ino.checked_sub(1)? as usize)?;
        (!matches!(node.kind, Kind::Gone)).then_some(node)
    }

    pub fn get_mut(&mut self, ino: u64) -> Option<&mut Node> {
        let node = self.nodes.get_mut(ino.checked_sub(1)? as usize)?;
        (!matches!(node.kind, Kind::Gone)).then_some(node)
    }

    pub fn lookup(&self, parent: u64, name: &[u8]) -> Option<u64> {
        Some(*self.get(parent)?.children()?.get(name)?)
    }

    /// Adds a node that has no name yet, which [`Tree::link`] then gives it.
    pub fn push(&mut self, node: Node) -> u64 {
        self.nodes.push(node);
        self.nodes.len() as u64
    }

    /// Adds a name for `ino` under `parent`, or `None` when `parent` is not a
    /// directory or already holds the name.
    pub fn link(&mut self, parent: u64, name: Vec<u8>, ino: u64) -> Option<()> {
        match &mut self.get_mut(parent)?.kind {
            Kind::Directory { children, .. } if !children.contains_key(&name) => {
                children.insert(name, ino);
            }
            _ => return None,
        }
        match &mut self.get_mut(ino)?.kind {
            // The two a directory starts with count its own name, so linking
            // it in adds nothing to it; its `..` is a name for its parent.
            Kind::Directory { parent: at, .. } => {
                *at = parent;
                self.get_mut(parent)?.nlink += 1;
            }
            _ => self.get_mut(ino)?.nlink += 1,
        }
        Some(())
    }

    /// Takes a name away from the directory holding it, returning the inode it
    /// named and whether that was its last name.
    pub fn unlink(&mut self, parent: u64, name: &[u8]) -> Option<(u64, bool)> {
        let ino = self.detach(parent, name)?;
        // A directory has the one name, so taking it away is the last of it.
        let last = self.get(ino)?.is_directory() || self.get(ino)?.nlink == 0;
        if last {
            self.get_mut(ino)?.kind = Kind::Gone;
        }
        Some((ino, last))
    }

    /// Moves a name from one directory to another. The inode is never without
    /// a name in between, so nothing it holds is lost on the way.
    pub fn rename(
        &mut self,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
    ) -> Option<u64> {
        if !self.get(new_parent)?.is_directory()
            || self.lookup(new_parent, new_name).is_some()
            || self.lookup(parent, name).is_none()
        {
            return None;
        }
        let ino = self.detach(parent, name)?;
        self.link(new_parent, new_name.to_vec(), ino)?;
        Some(ino)
    }

    /// Removes a name without deciding what that means for what it named.
    fn detach(&mut self, parent: u64, name: &[u8]) -> Option<u64> {
        let ino = self.lookup(parent, name)?;
        let directory = self.get(ino)?.is_directory();
        match &mut self.get_mut(parent)?.kind {
            Kind::Directory { children, .. } => children.remove(name),
            _ => return None,
        };
        if directory {
            self.get_mut(parent)?.nlink -= 1;
        } else {
            self.get_mut(ino)?.nlink -= 1;
        }
        Some(ino)
    }

    /// How many inodes have ever been made, which is what `statfs` reports.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Links `ino` in under its parent and records the path, so that whatever
    /// is placed inside it can find it.
    fn attach(&mut self, by_path: &mut HashMap<Vec<u8>, u64>, path: &[u8], ino: u64) -> Option<()> {
        let (parent, name) = split(path)?;
        let parent = *by_path.get(parent)?;
        self.link(parent, name.to_vec(), ino)?;
        by_path.insert(path.to_vec(), ino);
        Some(())
    }
}

/// Splits a canonical entry path into its parent's path and its own name.
/// The rootfs is spelled `.`, so anything directly inside it has no separator.
fn split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() || path == ROOT_ENTRY {
        return None;
    }
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(at) => Some((&path[..at], &path[at + 1..])),
        None => Some((ROOT_ENTRY, path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::{Entry, Kind as EntryKind};

    fn entry(kind: EntryKind, path: &str, link: &str, size: u64) -> Entry {
        Entry {
            kind,
            mode: 0o644,
            mtime: 7,
            offset: 0,
            size,
            path: path.as_bytes().to_vec(),
            link: link.as_bytes().to_vec(),
            xattrs: Vec::new(),
        }
    }

    /// One layer holding `etc/`, `etc/passwd`, `etc/link -> passwd` and
    /// `etc/same`, hard linked to `etc/passwd`.
    fn image() -> (Vec<(Vec<u8>, u32)>, Vec<Table>, Work) {
        let table = Table {
            entries: vec![
                entry(EntryKind::Directory, "etc", "", 0),
                entry(EntryKind::File, "etc/passwd", "", 11),
                entry(EntryKind::Symlink, "etc/link", "passwd", 0),
                entry(EntryKind::HardLink, "etc/same", "etc/passwd", 0),
            ],
        };
        let directories = vec![(b"etc".to_vec(), 0o755)];
        let work = Work {
            files: vec![vec![1]],
            symlinks: vec![(0, 2)],
            hard_links: vec![(0, 3)],
        };
        (directories, vec![table], work)
    }

    #[test]
    fn the_namespace_is_the_one_the_plan_describes() {
        let (directories, tables, work) = image();
        let (tree, _) = Tree::build(&directories, &tables, &work).expect("tree");

        let etc = tree.lookup(ROOT, b"etc").expect("etc");
        let passwd = tree.lookup(etc, b"passwd").expect("passwd");
        assert!(tree.get(etc).expect("node").is_directory());
        assert!(matches!(
            tree.get(passwd).expect("node").kind,
            Kind::File(Content::Layer(Body {
                layer: 0,
                offset: 0,
                size: 11
            }))
        ));
        assert!(matches!(
            &tree.get(tree.lookup(etc, b"link").expect("link")).expect("node").kind,
            Kind::Symlink(target) if target == b"passwd"
        ));
    }

    #[test]
    fn a_hard_link_is_another_name_for_the_same_inode() {
        let (directories, tables, work) = image();
        let (tree, _) = Tree::build(&directories, &tables, &work).expect("tree");
        let etc = tree.lookup(ROOT, b"etc").expect("etc");
        let passwd = tree.lookup(etc, b"passwd").expect("passwd");

        assert_eq!(tree.lookup(etc, b"same"), Some(passwd));
        assert_eq!(tree.get(passwd).expect("node").nlink, 2);
    }

    #[test]
    fn a_subdirectory_is_a_name_for_its_parent() {
        let directories = vec![(b"usr".to_vec(), 0o755), (b"usr/bin".to_vec(), 0o755)];
        let (tree, _) = Tree::build(&directories, &[], &Work::default()).expect("tree");

        let usr = tree.lookup(ROOT, b"usr").expect("usr");
        let bin = tree.lookup(usr, b"bin").expect("bin");
        assert_eq!(tree.get(ROOT).expect("root").nlink, 3);
        assert_eq!(tree.get(usr).expect("usr").nlink, 3);
        assert_eq!(tree.get(bin).expect("bin").nlink, 2);
        assert_eq!(tree.get(bin).expect("bin").parent(), Some(usr));
    }

    #[test]
    fn an_entry_with_no_directory_to_go_in_refuses_the_image() {
        let table = Table {
            entries: vec![entry(EntryKind::File, "etc/passwd", "", 1)],
        };
        let work = Work {
            files: vec![vec![0]],
            ..Work::default()
        };
        assert!(Tree::build(&[], &[table], &work).is_none());
    }

    #[test]
    fn a_hard_link_naming_nothing_refuses_the_image() {
        let table = Table {
            entries: vec![entry(EntryKind::HardLink, "etc/same", "etc/passwd", 0)],
        };
        let work = Work {
            files: vec![Vec::new()],
            hard_links: vec![(0, 0)],
            ..Work::default()
        };
        assert!(Tree::build(&[(b"etc".to_vec(), 0o755)], &[table], &work).is_none());
    }

    #[test]
    fn the_last_name_taken_away_takes_the_inode_with_it() {
        let (directories, tables, work) = image();
        let (mut tree, _) = Tree::build(&directories, &tables, &work).expect("tree");
        let etc = tree.lookup(ROOT, b"etc").expect("etc");
        let passwd = tree.lookup(etc, b"passwd").expect("passwd");

        assert_eq!(tree.unlink(etc, b"same"), Some((passwd, false)));
        assert!(tree.get(passwd).is_some(), "the other name still holds it");
        assert_eq!(tree.unlink(etc, b"passwd"), Some((passwd, true)));
        assert!(tree.get(passwd).is_none());
        assert!(tree.lookup(etc, b"passwd").is_none());
    }

    #[test]
    fn the_root_takes_the_mode_the_image_gives_it() {
        let (tree, _) =
            Tree::build(&[(b".".to_vec(), 0o750)], &[], &Work::default()).expect("tree");
        assert_eq!(tree.get(ROOT).expect("root").mode, 0o750);
    }
}
