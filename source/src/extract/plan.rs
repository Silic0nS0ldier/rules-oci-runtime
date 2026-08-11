//! Deciding what an image's layers actually need to place.
//!
//! A path a later layer overwrites, or a whiteout removes, is written and then
//! thrown away. On a 22 layer image that is most of a gigabyte of writes whose
//! only lasting effect is to be replaced. The entry tables recorded beside the
//! checkpoint indexes say what every layer holds, so the whole image can be
//! resolved before the first byte is written and the doomed entries skipped.
//!
//! Skipping is decided on the paths as the layers spell them, while extraction
//! still resolves those paths against the filesystem it is building. That is
//! sound in the one direction it is used: if two layers name the same path and
//! the later one is placed, the earlier one cannot survive. Either both names
//! resolve to the same file, and the later write replaces it, or something
//! between them was replaced to make them differ, and that replacement removed
//! the earlier file anyway.
//!
//! It is not sound in the other direction, so nothing here decides *where* an
//! entry goes, only whether it is worth placing at all.

use std::collections::{BTreeMap, HashMap, HashSet};

use camino::Utf8Path;

use crate::entries::{Entry, Kind, Table};
use crate::fsutil;
use crate::image::{Descriptor, parse_digest};
use crate::log::log;

use super::whiteout::{self, Whiteout};

/// What `create_dir` would have given a directory nothing names.
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;

/// The rootfs itself, which a layer names as `.` or `/` and which the tree
/// holds the one way.
const ROOT_ENTRY: &[u8] = b".";

/// The entries each layer can skip, keyed by layer digest, and the directory
/// tree they all need.
#[derive(Default)]
pub struct Plan {
    shadowed: HashMap<String, HashSet<Vec<u8>>>,
    directories: Vec<(Vec<u8>, u32)>,
    /// False when the image could not be resolved, and every layer therefore
    /// has to be placed in full.
    resolved: bool,
    /// What survives, for a caller that means to place it directly rather than
    /// by walking the layers. `None` when something in the image rules that
    /// out; see [`Plan::placeable`].
    work: Option<Work>,
    tables: Vec<Table>,
}

/// The surviving entries, grouped the way they have to be placed.
#[derive(Default)]
pub struct Work {
    /// Regular files, by layer, ordered by where their bodies sit in the
    /// layer's uncompressed stream.
    pub files: Vec<Vec<u32>>,
    /// Symlinks, shallowest path first, so one standing under another has
    /// somewhere to be.
    pub symlinks: Vec<(u32, u32)>,
    /// Hard links, which need the files they name to be on disk already.
    pub hard_links: Vec<(u32, u32)>,
}

impl Plan {
    /// Resolves the image, or returns an empty plan when it cannot be.
    ///
    /// Every layer has to be accounted for: one missing table means unknown
    /// entries, and an entry that is not known about cannot be shown to be
    /// safely skippable. Planning is an optimisation, so falling back to
    /// placing everything is always available.
    pub fn build(index_dir: Option<&Utf8Path>, layers: &[Descriptor]) -> Plan {
        let Some(dir) = index_dir else {
            return Plan::default();
        };
        let mut tables = Vec::with_capacity(layers.len());
        for layer in layers {
            let Ok(digest) = parse_digest(&layer.digest) else {
                return Plan::default();
            };
            let path = crate::sidecar::entries_at(dir, &digest.hex);
            match crate::sidecar::read(&path, Table::read_from) {
                Some(table) => tables.push(table),
                None => return Plan::default(),
            }
        }
        Plan::resolve(layers, tables)
    }

    fn resolve(layers: &[Descriptor], mut tables: Vec<Table>) -> Plan {
        // The paths the walk resolves and the paths the tables spell have to
        // be one and the same, or the tree below describes an image nobody
        // extracts.
        if !canonicalise(&mut tables) {
            return Plan::default();
        }

        // A hard link is made against what is already on disk, so whatever it
        // names has to be placed even where a later layer replaces it. There
        // are single figures of these in a real image.
        let linked: HashSet<&[u8]> = tables
            .iter()
            .flat_map(|table| &table.entries)
            .filter(|entry| entry.kind == Kind::HardLink)
            .map(|entry| entry.link.as_slice())
            .collect();

        let (tree, blocked) = replay(&tables);
        // The walk stops where a layer names something it cannot place, and a
        // plan that skipped that entry would sail past it. Nothing here is
        // worth an image that extracts differently depending on the sidecars
        // it happens to have.
        if blocked == Blocked::TheWalkStopsHere {
            return Plan::default();
        }

        let mut shadowed: HashMap<String, HashSet<Vec<u8>>> = HashMap::new();
        let mut total = 0;
        let mut bytes = 0u64;
        for (l, table) in tables.iter().enumerate() {
            let mut doomed = HashSet::new();
            for entry in table.entries.iter() {
                // Only bodies are worth skipping, and a whiteout marker has to
                // reach the extractor or nothing gets removed.
                if !entry.kind.is_file() || whiteout::of(&entry.path).is_some() {
                    continue;
                }
                if linked.contains(entry.path.as_slice()) {
                    continue;
                }
                // Whether the layer wins the path, not whether this entry
                // does: a layer that names one path twice is skipped by path,
                // so shadowing the loser would take the winner with it.
                let survives = tree
                    .get(entry.path.as_slice())
                    .is_some_and(|node| node.owner.0 == l);
                if !survives {
                    bytes += entry.size;
                    doomed.insert(entry.path.clone());
                }
            }
            total += doomed.len();
            if !doomed.is_empty() {
                shadowed.insert(layers[l].digest.clone(), doomed);
            }
        }
        if total > 0 {
            log!(
                "Skipping {total} files ({} MiB) that later layers replace",
                bytes >> 20
            );
        }
        let directories = directories(&tree);
        let work = (blocked == Blocked::Nothing)
            .then(|| placeable(&tree, &tables))
            .flatten();
        Plan {
            shadowed,
            directories,
            resolved: true,
            work,
            tables,
        }
    }

    /// True when the whole image was resolved, so the directory tree below is
    /// the one it ends up with and the layers need not build it themselves.
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    /// The directories the image needs, parents before children.
    pub fn directories(&self) -> &[(Vec<u8>, u32)] {
        &self.directories
    }

    /// What survives, when the image is one that can be placed straight from
    /// the plan rather than by walking its layers.
    pub fn work(&self) -> Option<&Work> {
        self.work.as_ref()
    }

    pub fn table(&self, layer: usize) -> &Table {
        &self.tables[layer]
    }

    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// True when this layer's copy of `path` is replaced or removed by a later
    /// one, and so never has to be written.
    pub fn is_shadowed(&self, digest: &str, path: &[u8]) -> bool {
        self.shadowed
            .get(digest)
            .is_some_and(|doomed| doomed.contains(path))
    }
}

/// Rewrites every path in every table into the form the walk would resolve it
/// to, so that `./etc/x` and `etc/x` are one path here as they are on disk.
///
/// False when a layer names something the walk refuses, or names the rootfs
/// itself as anything but a directory. Both are errors, and the walk is where
/// they are raised: planning only ever decides what need not be done.
fn canonicalise(tables: &mut [Table]) -> bool {
    let mut canonical = Vec::new();
    for table in tables {
        for entry in &mut table.entries {
            if fsutil::names_the_root(&entry.path) {
                if entry.kind != Kind::Directory {
                    return false;
                }
                // The rootfs is already there and only its mode is deferred,
                // so it is spelled the one way the tree can hold.
                entry.path.clear();
                entry.path.extend_from_slice(ROOT_ENTRY);
                continue;
            }
            if !fsutil::canonical_entry_path(&entry.path, &mut canonical) {
                return false;
            }
            std::mem::swap(&mut entry.path, &mut canonical);
            // A symlink target is followed from where the link stands rather
            // than from the rootfs, so it is left as the layer spelled it.
            if entry.kind == Kind::HardLink {
                if !fsutil::canonical_entry_path(&entry.link, &mut canonical) {
                    return false;
                }
                std::mem::swap(&mut entry.link, &mut canonical);
            }
        }
    }
    true
}

/// Groups the surviving entries for a caller that will place them directly.
///
/// Returns `None` where the image holds something that only a walk of the
/// layers can deal with:
///
/// - a sparse file, whose body is not a flat run of the stream, so nothing can
///   be written from an offset and a length;
/// - an entry that resolves through a symlink, where the path the layer spells
///   is not the path it lands on, and working that out means resolving against
///   the tree as it is built;
/// - a hard link naming something the final tree does not hold, or holds
///   behind a symlink, where linking would follow the symlink to wherever it
///   points and the walk checks that against the rootfs as it goes;
/// - a hard link whose target is replaced after it, where the copy the walk
///   linked is not the copy that survives.
fn placeable(tree: &BTreeMap<&[u8], Node>, tables: &[Table]) -> Option<Work> {
    let mut work = Work {
        files: vec![Vec::new(); tables.len()],
        ..Work::default()
    };
    for (path, node) in tree {
        let (layer, entry) = node.owner;
        match node.kind {
            // Nothing here is created under something that is not a directory,
            // so whatever is there has to be resolved against the tree as it
            // is built. That is the walk's job for the whole image.
            _ if behind_a_link(tree, path) => return None,
            Kind::Directory | Kind::Unsupported => continue,
            Kind::Sparse => return None,
            Kind::File => work.files[layer].push(entry as u32),
            // A `BTreeMap` orders by path, so a link under another link is
            // already after it.
            Kind::Symlink => work.symlinks.push((layer as u32, entry as u32)),
            Kind::HardLink => {
                let target = tables[layer].entries[entry].link.as_slice();
                let at = tree.get(target)?;
                if behind_a_link(tree, target) {
                    return None;
                }
                // The walk links the copy standing at the target when the
                // entry appears, and a later layer replacing that path
                // unlinks it rather than writing through it. Links are placed
                // here only once every file is on disk, so the copy that
                // survives is the only one there is to link.
                if at.owner > (layer, entry) {
                    return None;
                }
                work.hard_links.push((layer as u32, entry as u32))
            }
        }
    }
    for (layer, entries) in work.files.iter_mut().enumerate() {
        entries.sort_unstable_by_key(|&e| tables[layer].entries[e as usize].offset);
    }
    // In the order the layers named them, so a link naming another link is
    // made after it, as it was on the walk.
    work.hard_links.sort_unstable();
    Some(work)
}

/// What the layers leave at a path once all of them have been applied.
struct Node {
    kind: Kind,
    /// The layer and entry that placed what is here.
    owner: (usize, usize),
    mode: u32,
    /// The last layer with an entry naming this path, which is not always the
    /// one that placed what is here: a directory entry can leave a standing
    /// symlink alone and still name the path. `None` for a directory nobody
    /// named, made only to hold something else.
    named_by: Option<usize>,
}

/// Replays every layer in order into the tree they build between them.
///
/// This mirrors what the extractor does to the filesystem, entry for entry,
/// which is what makes the result something the extractor can be held to:
/// a directory keeps a symlink already standing where it wants to be, and
/// anything else takes over the path it names and everything underneath it.
///
/// Also reports what stood in the way of the entries as they were placed. The
/// tree says where the image ends up, not what it went through to get there:
/// a layer writing under a path that is a file at the time stops the walk,
/// even where a later layer makes a directory of it again.
///
/// Sorted by path, so everything under a directory is one contiguous range and
/// a removal costs what it removes rather than a pass over the whole image.
fn replay(tables: &[Table]) -> (BTreeMap<&[u8], Node>, Blocked) {
    let mut tree: BTreeMap<&[u8], Node> = BTreeMap::new();
    let mut bound = Vec::new();
    let mut blocked = Blocked::Nothing;
    for (l, table) in tables.iter().enumerate() {
        for (e, entry) in table.entries.iter().enumerate() {
            // Markers as well as entries: the walk asks the filesystem about
            // the path either way, and asking underneath a file is what stops
            // it.
            blocked = blocked.worst(under(&tree, tables, &entry.path));
            match whiteout::of(&entry.path) {
                // An opaque marker empties the directory of what the layers
                // below put there, so where it sits in its own layer does not
                // matter.
                Some(Whiteout::Opaque(dir)) => remove_under(
                    &mut tree,
                    &dir,
                    &mut bound,
                    Removal::LowerLayers { layer: l },
                ),
                Some(Whiteout::Named(target)) => {
                    // A path this layer placed is not hidden by its own
                    // layer's marker, and neither is anything under it: the
                    // walk sees the path in what it has written and leaves the
                    // marker alone entirely.
                    let ours = tree
                        .get(target.as_slice())
                        .is_some_and(|node| node.named_by == Some(l));
                    if !ours {
                        tree.remove(target.as_slice());
                        // What is underneath goes with it, this layer's own
                        // work included, because the walk removes the named
                        // path whole.
                        remove_under(&mut tree, &target, &mut bound, Removal::Whole);
                    }
                }
                // A marker naming nothing removes nothing, and the walk
                // refuses the image over it.
                marker => {
                    if marker.is_some() {
                        blocked = Blocked::TheWalkStopsHere;
                    }
                    blocked = blocked.worst(links_somewhere(&tree, tables, entry));
                    let node = Node {
                        kind: entry.kind,
                        owner: (l, e),
                        mode: entry.mode,
                        named_by: Some(l),
                    };
                    match entry.kind {
                        // The extractor warns and moves on, leaving the path
                        // as it found it.
                        Kind::Unsupported => {}
                        Kind::Directory => {
                            ensure_parents(&mut tree, &entry.path, (l, e));
                            // `prepare_directory` keeps a symlink that already
                            // resolves to a directory and replaces one that
                            // does not, so which it is decides the path.
                            let standing = tree.get(entry.path.as_slice());
                            let keep = match standing.filter(|at| at.kind == Kind::Symlink) {
                                None => false,
                                Some(at) => match resolves_to_a_directory(
                                    &tree,
                                    tables,
                                    entry.path.as_slice(),
                                    at,
                                ) {
                                    Some(resolves) => resolves,
                                    // Not something the plan can follow. The
                                    // link is left where it is, which creates
                                    // nothing over it either way.
                                    None => {
                                        blocked = blocked.worst(Blocked::ThroughASymlink);
                                        true
                                    }
                                },
                            };
                            if !keep {
                                tree.insert(&entry.path, node);
                            } else if let Some(at) = tree.get_mut(entry.path.as_slice()) {
                                // The link stays, but the path has been named
                                // again, which is what a whiteout later in
                                // this layer goes by.
                                at.named_by = Some(l);
                            }
                        }
                        // Everything else clears the path first, which takes
                        // the tree underneath it as well.
                        _ => {
                            ensure_parents(&mut tree, &entry.path, (l, e));
                            remove_under(&mut tree, &entry.path, &mut bound, Removal::Whole);
                            tree.insert(&entry.path, node);
                        }
                    }
                }
            }
        }
    }
    (tree, blocked)
}

/// Whether the symlink standing at `path` resolves to a directory, or `None`
/// when the plan cannot say: a target it will not resolve, or one that lands
/// on another symlink.
fn resolves_to_a_directory(
    tree: &BTreeMap<&[u8], Node>,
    tables: &[Table],
    path: &[u8],
    at: &Node,
) -> Option<bool> {
    let target = &tables[at.owner.0].entries[at.owner.1].link;
    let mut joined = Vec::new();
    // An absolute target is rooted at the rootfs; a relative one starts from
    // the directory the link sits in.
    if !target.starts_with(b"/")
        && let Some(slash) = path.iter().rposition(|&byte| byte == b'/')
    {
        joined.extend_from_slice(&path[..slash + 1]);
    }
    joined.extend_from_slice(target);

    let mut resolved = Vec::new();
    if !fsutil::canonical_entry_path(&joined, &mut resolved) {
        return None;
    }
    match tree.get(resolved.as_slice()).map(|node| node.kind) {
        Some(Kind::Symlink) => None,
        Some(kind) => Some(kind == Kind::Directory),
        // Nothing names it, but it is still a directory when something is
        // under it: the walk creates the parents of what it places.
        None => Some(holds_anything(tree, &resolved)),
    }
}

/// True when the tree holds anything under `dir`, which makes it a directory
/// even though no entry names it.
fn holds_anything(tree: &BTreeMap<&[u8], Node>, dir: &[u8]) -> bool {
    let mut bound = dir.to_vec();
    bound.push(b'/');
    tree.range::<[u8], _>((
        std::ops::Bound::Included(bound.as_slice()),
        std::ops::Bound::Unbounded,
    ))
    .next()
    .is_some_and(|(path, _)| path.starts_with(&bound))
}

/// What stands between the walk and `path`, if anything.
///
/// A symlink that resolves to a directory is one the walk writes through; one
/// that resolves to nothing is one it stops on, which is the difference
/// between an image it extracts and an image it refuses.
fn under(tree: &BTreeMap<&[u8], Node>, tables: &[Table], path: &[u8]) -> Blocked {
    let Some((ancestor, node)) = blocked_by(tree, path) else {
        return Blocked::Nothing;
    };
    if node.kind != Kind::Symlink {
        return Blocked::TheWalkStopsHere;
    }
    match resolves_to_a_directory(tree, tables, ancestor, node) {
        Some(true) => Blocked::ThroughASymlink,
        _ => Blocked::TheWalkStopsHere,
    }
}

/// Whether a link entry names something the walk can link to.
fn links_somewhere(tree: &BTreeMap<&[u8], Node>, tables: &[Table], entry: &Entry) -> Blocked {
    match entry.kind {
        // A link naming nothing is refused rather than left pointing at
        // whatever the empty name resolves to.
        Kind::Symlink | Kind::HardLink if entry.link.is_empty() => Blocked::TheWalkStopsHere,
        // A hard link is made against what is on disk, which is what the tree
        // holds at this point. A directory cannot be linked, and nothing was
        // placed for a type the extractor does not support. Nor can a link
        // name itself or anything under itself: the walk clears the path
        // before linking, which takes the copy it was about to link to.
        Kind::HardLink
            if names_itself(&entry.path, &entry.link)
                || under(tree, tables, &entry.link) != Blocked::Nothing
                || !tree.get(entry.link.as_slice()).is_some_and(|node| {
                    !matches!(node.kind, Kind::Directory | Kind::Unsupported)
                }) =>
        {
            Blocked::TheWalkStopsHere
        }
        _ => Blocked::Nothing,
    }
}

/// Records the directories the walk creates to hold an entry.
///
/// They are nobody's entry, so nothing names them and nothing gives them a
/// mode, but they stay behind once whatever they were made for is gone --
/// which is why the tree has to hold them rather than work them out from what
/// survives.
fn ensure_parents<'a>(tree: &mut BTreeMap<&'a [u8], Node>, path: &'a [u8], owner: (usize, usize)) {
    for ancestor in ancestors(path) {
        // Something already there is either the directory this needs or a
        // symlink the walk writes through, and neither is ours to replace.
        tree.entry(ancestor).or_insert(Node {
            kind: Kind::Directory,
            owner,
            mode: DEFAULT_DIRECTORY_MODE,
            named_by: None,
        });
    }
}

/// True when `target` is `path` or something under it.
fn names_itself(path: &[u8], target: &[u8]) -> bool {
    target == path || (target.starts_with(path) && target.get(path.len()) == Some(&b'/'))
}

/// The directories the final tree needs, parents before children.
///
/// A path that ends up a symlink is left out however many entries sit under
/// it: what it points at is where they actually go, and creating a directory
/// there would displace the link.
fn directories(tree: &BTreeMap<&[u8], Node>) -> Vec<(Vec<u8>, u32)> {
    let mut wanted: BTreeMap<&[u8], u32> = BTreeMap::new();
    for (path, node) in tree {
        if node.kind == Kind::Directory && !behind_a_link(tree, path) {
            wanted.insert(path, node.mode);
        }
        // Ancestors nothing names still have to exist. `create_dir` would have
        // made them with the default mode, so they get it here too.
        for ancestor in ancestors(path) {
            if tree.contains_key(ancestor)
                || wanted.contains_key(ancestor)
                || behind_a_link(tree, ancestor)
            {
                continue;
            }
            wanted.insert(ancestor, DEFAULT_DIRECTORY_MODE);
        }
    }
    // A `BTreeMap` orders by path, so a parent always precedes what is under
    // it and the tree can be created in one pass.
    wanted
        .into_iter()
        .map(|(path, mode)| (path.to_vec(), mode))
        .collect()
}

/// Every strict ancestor of `path`, shallowest first.
fn ancestors(path: &[u8]) -> impl Iterator<Item = &[u8]> {    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(move |(at, _)| &path[..at])
        .filter(|ancestor| !ancestor.is_empty())
}

/// True when something that is not a directory stands on the way to `path`.
///
/// Whatever that is resolves somewhere else, and it is not created here, so
/// there would be nothing to create this under. The extractor resolves the
/// entries that live there against the tree as it builds it, as it always has.
fn behind_a_link(tree: &BTreeMap<&[u8], Node>, path: &[u8]) -> bool {
    blocked_by(tree, path).is_some()
}

/// The first thing on the way to `path` that is not a directory.
fn blocked_by<'a, 'p>(
    tree: &'a BTreeMap<&'p [u8], Node>,
    path: &'p [u8],
) -> Option<(&'p [u8], &'a Node)> {
    ancestors(path).find_map(|ancestor| {
        let node = tree.get(ancestor)?;
        (node.kind != Kind::Directory).then_some((ancestor, node))
    })
}

/// What replaying the layers found in the way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blocked {
    /// Nothing: every entry had a directory to go in and named something the
    /// walk could place.
    Nothing,
    /// Something was written through a symlink. The walk follows it to
    /// wherever it points and the image still extracts; the plan cannot say
    /// where that is.
    ThroughASymlink,
    /// An entry the walk stops on: a marker naming nothing, a link naming
    /// nothing, a hard link naming what is not on disk yet, or anything under
    /// a path that was not a directory at the time.
    ///
    /// These are judged over every entry rather than over the tree the layers
    /// leave behind, because the entry that stops the walk is often one a
    /// later layer replaces -- and a plan that skipped it would sail past
    /// something the walk refuses.
    TheWalkStopsHere,
}

impl Blocked {
    fn worst(self, other: Blocked) -> Blocked {
        match (self, other) {
            (Blocked::TheWalkStopsHere, _) | (_, Blocked::TheWalkStopsHere) => {
                Blocked::TheWalkStopsHere
            }
            (Blocked::ThroughASymlink, _) | (_, Blocked::ThroughASymlink) => {
                Blocked::ThroughASymlink
            }
            _ => Blocked::Nothing,
        }
    }
}

/// Why something under a path is being dropped.
#[derive(Clone, Copy)]
enum Removal {
    /// The path is being taken over, so the walk clears it whole and this
    /// layer's own work under it goes with everything else.
    Whole,
    /// An opaque marker: what the layers below left goes, and what this layer
    /// put there stays, along with the directories holding it.
    LowerLayers { layer: usize },
}

/// Drops what `removal` says goes from beneath `dir`, without touching a
/// sibling whose name merely starts the same way.
///
/// An empty `dir` is the rootfs itself, which every path is under. The rootfs
/// is not removed with them: the walk empties the directory the marker names
/// rather than deleting it.
///
/// Every entry that is not a directory replays through here, so the bound it
/// seeks from is built in a buffer the caller keeps rather than a fresh pair
/// of allocations per path.
fn remove_under(
    tree: &mut BTreeMap<&[u8], Node>,
    dir: &[u8],
    bound: &mut Vec<u8>,
    removal: Removal,
) {
    bound.clear();
    if !dir.is_empty() {
        bound.extend_from_slice(dir);
        bound.push(b'/');
    }

    // Everything under `dir` sorts from the bound onwards and is contiguous,
    // so the first path that is not under it ends the range.
    let under: Vec<(&[u8], Option<usize>)> = tree
        .range::<[u8], _>((
            std::ops::Bound::Included(bound.as_slice()),
            std::ops::Bound::Unbounded,
        ))
        .take_while(|(path, _)| path.starts_with(bound.as_slice()))
        .filter(|(path, _)| **path != ROOT_ENTRY)
        .map(|(path, node)| (*path, node.named_by))
        .collect();

    let mut doomed: Vec<&[u8]> = Vec::new();
    match removal {
        Removal::Whole => doomed.extend(under.iter().map(|(path, _)| *path)),
        Removal::LowerLayers { layer } => {
            // Read deepest first, so the last path this layer named is the
            // one to ask about: everything under a directory sits directly
            // before it.
            let mut ours: Option<&[u8]> = None;
            for (path, named_by) in under.iter().rev() {
                if *named_by == Some(layer) {
                    ours = Some(path);
                    continue;
                }
                if !ours.is_some_and(|kept| names_itself(path, kept)) {
                    doomed.push(path);
                }
            }
        }
    }
    for path in doomed {
        tree.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::Entry;

    fn descriptor(n: u8) -> Descriptor {
        Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: format!("sha256:{:064x}", n),
            size: 0,
            platform: None,
        }
    }

    fn file(path: &str) -> Entry {
        Entry {
            kind: Kind::File,
            mode: 0o644,
            mtime: 0,
            offset: 0,
            size: 1,
            path: path.as_bytes().to_vec(),
            link: Vec::new(),
            xattrs: Vec::new(),
        }
    }

    fn of_kind(path: &str, kind: Kind) -> Entry {
        Entry {
            kind,
            mode: if kind == Kind::Directory { 0o755 } else { 0o644 },
            ..file(path)
        }
    }

    fn dir(path: &str) -> Entry {
        of_kind(path, Kind::Directory)
    }

    fn symlink(path: &str, target: &str) -> Entry {
        Entry {
            link: target.as_bytes().to_vec(),
            ..of_kind(path, Kind::Symlink)
        }
    }

    fn layer(entries: Vec<Entry>) -> Table {
        Table { entries }
    }

    /// The paths the plan would create as directories, for readability.
    fn dirs_of(layers: Vec<Table>) -> Vec<String> {
        let (plan, _) = plan_of(layers);
        plan.directories()
            .iter()
            .map(|(path, _)| String::from_utf8_lossy(path).into_owned())
            .collect()
    }

    fn table(paths: &[&str]) -> Table {
        Table {
            entries: paths.iter().map(|p| file(p)).collect(),
        }
    }

    fn plan_of(layers: Vec<Table>) -> (Plan, Vec<Descriptor>) {
        let descriptors: Vec<Descriptor> = (0..layers.len() as u8).map(descriptor).collect();
        (Plan::resolve(&descriptors, layers), descriptors)
    }

    #[test]
    fn a_later_layer_shadows_an_earlier_copy() {
        let (plan, d) = plan_of(vec![table(&["a", "keep"]), table(&["a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"a"));
        assert!(!plan.is_shadowed(&d[0].digest, b"keep"));
        assert!(!plan.is_shadowed(&d[1].digest, b"a"), "the winner stays");
    }

    /// Shadowing is recorded by path, so a layer naming one path twice would
    /// mark it doomed for the copy that loses and skip the copy that wins.
    #[test]
    fn a_path_a_layer_names_twice_is_not_shadowed_in_that_layer() {
        let (plan, d) = plan_of(vec![layer(vec![file("twice"), file("twice")])]);
        assert!(!plan.is_shadowed(&d[0].digest, b"twice"));
    }

    #[test]
    fn a_whiteout_shadows_what_it_hides() {
        let (plan, d) = plan_of(vec![table(&["dir/a", "dir/b", "other"]), table(&["dir/.wh.a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(!plan.is_shadowed(&d[0].digest, b"dir/b"));
        assert!(!plan.is_shadowed(&d[0].digest, b"other"));
    }

    /// The marker is itself a regular file entry, and the extractor has to see
    /// it or nothing is removed from the tree the lower layers built.
    #[test]
    fn a_whiteout_marker_is_never_shadowed() {
        let (plan, d) = plan_of(vec![table(&["dir/.wh.a"]), table(&["dir/.wh.a"])]);
        assert!(!plan.is_shadowed(&d[0].digest, b"dir/.wh.a"));
        assert!(!plan.is_shadowed(&d[1].digest, b"dir/.wh.a"));
    }

    /// An opaque whiteout empties a directory; it does not delete it. The
    /// directory sorts at the front of its own subtree, so clearing the
    /// subtree used to take it as well.
    #[test]
    fn an_opaque_whiteout_leaves_the_directory_it_empties() {
        assert_eq!(
            dirs_of(vec![
                layer(vec![dir("dir"), dir("dir/sub")]),
                layer(vec![dir("dir"), file("dir/.wh..wh..opq")]),
            ]),
            ["dir"],
            "the directory stays and only what was under it goes"
        );
    }

    #[test]
    fn an_opaque_whiteout_shadows_the_directory_it_names() {
        let (plan, d) = plan_of(vec![
            table(&["dir/a", "dir/sub/b", "dirty", "elsewhere"]),
            table(&["dir/.wh..wh..opq"]),
        ]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(plan.is_shadowed(&d[0].digest, b"dir/sub/b"));
        assert!(
            !plan.is_shadowed(&d[0].digest, b"dirty"),
            "a sibling that merely starts the same way is not underneath it"
        );
        assert!(!plan.is_shadowed(&d[0].digest, b"elsewhere"));
    }

    #[test]
    fn a_whiteout_of_a_directory_shadows_what_is_under_it() {
        let (plan, d) = plan_of(vec![table(&["dir/a", "dir", "dirty"]), table(&["\
.wh.dir"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        assert!(plan.is_shadowed(&d[0].digest, b"dir"));
        assert!(!plan.is_shadowed(&d[0].digest, b"dirty"));
    }

    /// A later layer restoring a path the whiteout removed keeps it.
    #[test]
    fn a_path_written_after_a_whiteout_survives() {
        let (plan, d) = plan_of(vec![table(&["a"]), table(&[".wh.a"]), table(&["a"])]);
        assert!(plan.is_shadowed(&d[0].digest, b"a"));
        assert!(!plan.is_shadowed(&d[2].digest, b"a"));
    }

    /// A hard link is made against the tree as it stands, so the copy it names
    /// has to be there even when a later layer replaces it.
    #[test]
    fn a_file_a_hard_link_names_is_never_shadowed() {
        let mut linking = table(&["link"]);
        linking.entries[0].kind = Kind::HardLink;
        linking.entries[0].link = b"target".to_vec();

        let (plan, d) = plan_of(vec![table(&["target"]), linking, table(&["target"])]);
        assert!(
            !plan.is_shadowed(&d[0].digest, b"target"),
            "the copy the link was made against has to be placed"
        );
    }

    /// The walk refuses a `.wh.` that names nothing, so the plan must not
    /// place it as an ordinary file and reach a different answer.
    #[test]
    fn a_whiteout_naming_nothing_is_not_placeable() {
        let (plan, _) = plan_of(vec![layer(vec![dir("d"), file("d/.wh.")])]);
        assert!(plan.work().is_none());
    }

    /// A sparse body is a map of segments rather than a flat run of the
    /// stream, so only `tar` can place it.
    #[test]
    fn a_sparse_entry_is_not_placeable() {
        let (plan, _) = plan_of(vec![layer(vec![of_kind("holey", Kind::Sparse)])]);
        assert!(plan.work().is_none());
    }

    #[test]
    fn an_absent_table_plans_nothing() {
        let plan = Plan::build(None, &[descriptor(0)]);
        assert!(!plan.is_shadowed(&descriptor(0).digest, b"anything"));
    }

    /// The tables spell a path as its layer does, while the walk resolves it.
    /// A plan built on the raw spelling would not see that these are one file,
    /// and the span route would then place both and collide.
    mod one_path_however_it_is_spelled {
        use super::*;

        #[test]
        fn a_later_layer_still_shadows_it() {
            let (plan, d) = plan_of(vec![table(&["./etc/config"]), table(&["/etc/config"])]);
            assert!(plan.is_shadowed(&d[0].digest, b"etc/config"));
            assert!(!plan.is_shadowed(&d[1].digest, b"etc/config"));
        }

        #[test]
        fn a_whiteout_still_hides_it() {
            let (plan, d) = plan_of(vec![table(&["./dir/a"]), table(&["dir/.wh.a"])]);
            assert!(plan.is_shadowed(&d[0].digest, b"dir/a"));
        }

        #[test]
        fn the_directory_it_lives_in_is_created_once() {
            assert_eq!(
                dirs_of(vec![
                    layer(vec![dir("./etc"), file("./etc/a")]),
                    layer(vec![dir("etc"), file("etc/b")]),
                ]),
                ["etc"]
            );
        }
    }

    /// Refusing an entry is the walk's job, so an image holding one is left to
    /// it rather than planned around.
    mod what_only_the_walk_can_judge {
        use super::*;

        #[test]
        fn a_path_that_climbs_out_of_the_rootfs() {
            let (plan, _) = plan_of(vec![table(&["../escaped"])]);
            assert!(!plan.is_resolved());
        }

        #[test]
        fn a_hard_link_naming_something_outside_the_rootfs() {
            let mut link = of_kind("stolen", Kind::HardLink);
            link.link = b"../../etc/passwd".to_vec();
            let (plan, _) = plan_of(vec![layer(vec![link])]);
            assert!(!plan.is_resolved());
        }

        /// Linking follows a symlink on the way to the target, out of the
        /// rootfs if that is where it points. The walk checks that against the
        /// tree it has built; the plan can only decline.
        #[test]
        fn a_hard_link_reaching_through_a_symlink() {
            let mut link = of_kind("stolen", Kind::HardLink);
            link.link = b"lnk/passwd".to_vec();
            let (plan, _) = plan_of(vec![layer(vec![symlink("lnk", "/etc"), link])]);
            assert!(plan.work().is_none());
        }

        #[test]
        fn a_hard_link_naming_a_path_the_image_never_places() {
            let mut link = of_kind("dangling", Kind::HardLink);
            link.link = b"absent".to_vec();
            let (plan, _) = plan_of(vec![layer(vec![link])]);
            assert!(plan.work().is_none());
        }

        /// The walk links the copy standing at the target when the entry
        /// appears, and a later layer replacing that path unlinks it rather
        /// than writing through it. Links are placed after every file, by
        /// which time only the surviving copy is there to link.
        #[test]
        fn a_hard_link_whose_target_a_later_layer_replaces() {
            let mut link = of_kind("link", Kind::HardLink);
            link.link = b"target".to_vec();
            let (plan, _) = plan_of(vec![
                layer(vec![file("target"), link]),
                layer(vec![file("target")]),
            ]);
            assert!(plan.work().is_none());
        }

        /// The same layer, later in it: the walk has already replaced the
        /// copy by the time it links, so this is the same case.
        #[test]
        fn a_hard_link_whose_target_its_own_layer_replaces_afterwards() {
            let mut link = of_kind("link", Kind::HardLink);
            link.link = b"target".to_vec();
            let (plan, _) = plan_of(vec![layer(vec![file("target"), link, file("target")])]);
            assert!(plan.work().is_none());
        }

        #[test]
        fn the_rootfs_named_as_anything_but_a_directory() {
            let (plan, _) = plan_of(vec![layer(vec![file(".")])]);
            assert!(!plan.is_resolved());
        }
    }

    /// A target replaced before the link is the copy the walk links, so there
    /// is nothing for the span route to get wrong.
    #[test]
    fn a_hard_link_made_against_the_surviving_copy_is_placeable() {
        let mut link = of_kind("link", Kind::HardLink);
        link.link = b"target".to_vec();
        let (plan, _) = plan_of(vec![
            layer(vec![file("target")]),
            layer(vec![file("target"), link]),
        ]);
        assert!(plan.work().is_some());
    }

    /// `tar -C dir .` writes the rootfs itself into every layer, so planning
    /// has to survive it.
    #[test]
    fn the_archive_root_is_planned_as_the_rootfs_itself() {
        let (plan, _) = plan_of(vec![layer(vec![dir("./"), dir("etc"), file("etc/a")])]);
        assert!(plan.is_resolved());
        assert!(plan.work().is_some());
    }

    /// Replacing a directory with anything else takes the tree under it away,
    /// exactly as `remove_any` does during extraction. Without this the plan
    /// would keep entries that no longer have anywhere to be, and once the
    /// directory tree is built up front they would be written through whatever
    /// took the directory's place.
    mod a_directory_replaced_by_something_else {
        use super::*;

        #[test]
        fn a_symlink_takes_the_tree_under_it() {
            let (plan, d) = plan_of(vec![
                layer(vec![dir("lib"), file("lib/a"), file("lib/sub/b"), file("libre")]),
                layer(vec![symlink("lib", "usr/lib")]),
            ]);
            assert!(plan.is_shadowed(&d[0].digest, b"lib/a"));
            assert!(plan.is_shadowed(&d[0].digest, b"lib/sub/b"));
            assert!(
                !plan.is_shadowed(&d[0].digest, b"libre"),
                "a sibling that merely starts the same way is untouched"
            );
        }

        #[test]
        fn a_file_takes_the_tree_under_it() {
            let (plan, d) = plan_of(vec![
                layer(vec![dir("d"), file("d/a")]),
                layer(vec![file("d")]),
            ]);
            assert!(plan.is_shadowed(&d[0].digest, b"d/a"));
        }

        #[test]
        fn a_hard_link_takes_the_tree_under_it() {
            let mut link = of_kind("d", Kind::HardLink);
            link.link = b"elsewhere".to_vec();
            let (plan, d) = plan_of(vec![
                layer(vec![dir("d"), file("d/a"), file("elsewhere")]),
                layer(vec![link]),
            ]);
            assert!(plan.is_shadowed(&d[0].digest, b"d/a"));
        }

        /// The path is gone, so nothing may be created there as a directory.
        #[test]
        fn the_replaced_path_is_not_created_as_a_directory() {
            let dirs = dirs_of(vec![
                layer(vec![dir("lib"), file("lib/a")]),
                layer(vec![symlink("lib", "usr/lib")]),
            ]);
            assert!(
                !dirs.iter().any(|d| d == "lib"),
                "a symlink must not be displaced by a directory: {dirs:?}"
            );
        }

        /// Restoring the directory afterwards puts everything back in play.
        #[test]
        fn a_directory_put_back_afterwards_is_a_directory_again() {
            let dirs = dirs_of(vec![
                layer(vec![dir("d"), file("d/a")]),
                layer(vec![file("d")]),
                layer(vec![dir("d"), file("d/b")]),
            ]);
            assert!(dirs.iter().any(|entry| entry == "d"), "{dirs:?}");

            let (plan, digests) = plan_of(vec![
                layer(vec![dir("d"), file("d/a")]),
                layer(vec![file("d")]),
                layer(vec![dir("d"), file("d/b")]),
            ]);
            assert!(plan.is_shadowed(&digests[0].digest, b"d/a"));
            assert!(!plan.is_shadowed(&digests[2].digest, b"d/b"));
        }

        /// Nothing under the replacement is created either. A directory there
        /// would have to be made through whatever took the path, which is not
        /// created here and so is not there yet to be followed.
        #[test]
        fn nothing_beneath_the_replacement_is_created_either() {
            let dirs = dirs_of(vec![
                layer(vec![symlink("lib", "usr/lib"), dir("usr"), dir("usr/lib")]),
                layer(vec![dir("lib/sub"), file("lib/sub/a")]),
            ]);
            assert!(
                !dirs.iter().any(|entry| entry.starts_with("lib")),
                "nothing may be created behind the link: {dirs:?}"
            );
            assert!(dirs.iter().any(|entry| entry == "usr/lib"), "{dirs:?}");
        }
    }

    /// A directory entry does not take a path back from a symlink that already
    /// resolves to a directory, which is what keeps `/lib -> usr/lib` standing
    /// when a later layer also ships `lib/`.
    #[test]
    fn a_directory_entry_leaves_a_standing_symlink_alone() {
        let dirs = dirs_of(vec![
            layer(vec![symlink("lib", "usr/lib"), dir("usr"), dir("usr/lib")]),
            layer(vec![dir("lib")]),
        ]);
        assert!(!dirs.iter().any(|d| d == "lib"), "{dirs:?}");
        assert!(dirs.iter().any(|d| d == "usr/lib"), "{dirs:?}");
    }

    #[test]
    fn directories_are_listed_parents_first() {
        let dirs = dirs_of(vec![layer(vec![dir("a/b/c"), file("a/b/c/d"), dir("a")])]);
        assert_eq!(dirs, ["a", "a/b", "a/b/c"], "including the ones nothing names");
    }

    #[test]
    fn a_whiteout_takes_the_directory_it_names_out_of_the_tree() {
        let dirs = dirs_of(vec![
            layer(vec![dir("gone"), file("gone/a"), dir("kept")]),
            layer(vec![file(".wh.gone")]),
        ]);
        assert_eq!(dirs, ["kept"]);
    }

    /// The marker names the rootfs, which is not a path under it, so the tree
    /// has no key to clear from and used to keep everything.
    #[test]
    fn an_opaque_whiteout_at_the_top_of_a_layer_clears_the_whole_tree() {
        let (plan, d) = plan_of(vec![
            table(&["a", "dir/b"]),
            layer(vec![file(".wh..wh..opq"), file("kept")]),
        ]);
        assert!(plan.is_shadowed(&d[0].digest, b"a"));
        assert!(plan.is_shadowed(&d[0].digest, b"dir/b"));
        assert!(!plan.is_shadowed(&d[1].digest, b"kept"));
    }

    /// It empties the rootfs rather than removing it, so the mode a layer
    /// gives the rootfs still stands.
    #[test]
    fn an_opaque_whiteout_at_the_top_of_a_layer_leaves_the_rootfs() {
        let dirs = dirs_of(vec![
            layer(vec![dir("."), dir("gone")]),
            layer(vec![file(".wh..wh..opq")]),
        ]);
        assert_eq!(dirs, ["."]);
    }
}
