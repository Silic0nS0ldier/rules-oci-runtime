//! The markers a layer uses to hide what the layers below it left behind.
//!
//! Both routes have to read them the same way: the plan decides what a marker
//! removes from the tree it resolves, and the walk decides what it removes
//! from the tree on disk. A marker only one of them recognised would leave the
//! two describing different images.

pub(super) const PREFIX: &[u8] = b".wh.";
pub(super) const OPAQUE: &[u8] = b".wh..wh..opq";

pub(super) enum Whiteout {
    /// `.wh..wh..opq`: hides everything the lower layers put in this
    /// directory. Empty when the marker sits at the top of the layer, which
    /// names the rootfs itself.
    Opaque(Vec<u8>),
    /// `.wh.name`: hides the one path it names.
    Named(Vec<u8>),
    /// `.wh.` with nothing after it, which names nothing to remove. Taking it
    /// as a name would leave it pointing at the directory the marker sits in.
    Invalid,
}

/// What `path` marks for removal, or `None` when it is an ordinary entry.
pub(super) fn of(path: &[u8]) -> Option<Whiteout> {
    let (dir, name) = split(path);
    if name == OPAQUE {
        return Some(Whiteout::Opaque(dir.to_vec()));
    }
    let target = name.strip_prefix(PREFIX)?;
    if target.is_empty() {
        return Some(Whiteout::Invalid);
    }
    // The marker sits in the middle of the path, so what it names has to be
    // put back together rather than sliced out.
    let mut named = Vec::with_capacity(dir.len() + 1 + target.len());
    if !dir.is_empty() {
        named.extend_from_slice(dir);
        named.push(b'/');
    }
    named.extend_from_slice(target);
    Some(Whiteout::Named(named))
}

fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&path[..0], path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_names_what_it_hides() {
        assert!(matches!(of(b"dir/.wh..wh..opq"), Some(Whiteout::Opaque(d)) if d == b"dir"));
        assert!(matches!(of(b"dir/.wh.name"), Some(Whiteout::Named(n)) if n == b"dir/name"));
        assert!(matches!(of(b".wh.name"), Some(Whiteout::Named(n)) if n == b"name"));
        assert!(of(b"dir/name").is_none());
    }

    /// A marker at the top of a layer names the rootfs, which is not a path
    /// under it and so has no name of its own.
    #[test]
    fn a_marker_at_the_top_of_a_layer_names_the_rootfs() {
        assert!(matches!(of(b".wh..wh..opq"), Some(Whiteout::Opaque(d)) if d.is_empty()));
    }

    #[test]
    fn a_marker_naming_nothing_is_invalid_rather_than_ordinary() {
        assert!(matches!(of(b"dir/.wh."), Some(Whiteout::Invalid)));
        assert!(matches!(of(b".wh."), Some(Whiteout::Invalid)));
    }

    #[test]
    fn the_opaque_marker_is_itself_a_whiteout_name() {
        assert!(OPAQUE.starts_with(PREFIX));
    }
}
