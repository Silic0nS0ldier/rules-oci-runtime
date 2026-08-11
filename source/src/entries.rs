//! A record of what a layer's tar stream contains and where.
//!
//! The checkpoint index in [`crate::zinfo`] says where to resume inflating a
//! blob; this says what is worth inflating. Knowing every entry's path and the
//! offset of its body up front is what lets the whole image be resolved before
//! anything is written: an entry a later layer overwrites, or a whiteout
//! removes, never has to be extracted at all.
//!
//! Built at Bazel build time alongside the checkpoint index, and read back as
//! a sidecar next to it. Both are optimisations, so a missing or unreadable
//! table means falling back to walking the layer, not failing.

use std::io::{self, Read, Write};

use crate::error::{Error, IoContext, Result};

const MAGIC: &[u8; 4] = b"OTE2";

/// A layer carries extended attributes as one PAX record per attribute.
const XATTR_PREFIX: &str = "SCHILY.xattr.";

/// A layer with more entries than this is not one we can plan for in bounded
/// memory, and is far past anything a real image contains.
const MAX_ENTRIES: u32 = 8 << 20;

/// What an entry asks to be placed. Types the extractor does not support are
/// recorded rather than dropped, so a plan built from the table can account
/// for a path the layer mentions even when nothing is written for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    HardLink,
    /// A file whose body is stored as a sparse map rather than laid out flat,
    /// so `offset` and `size` do not describe where its contents are. It takes
    /// a path like any other file, but only `tar` knows how to place it.
    Sparse,
    Unsupported,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::File => 0,
            Kind::Directory => 1,
            Kind::Symlink => 2,
            Kind::HardLink => 3,
            Kind::Unsupported => 4,
            Kind::Sparse => 5,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Kind::File,
            1 => Kind::Directory,
            2 => Kind::Symlink,
            3 => Kind::HardLink,
            4 => Kind::Unsupported,
            5 => Kind::Sparse,
            _ => return None,
        })
    }

    /// True when the entry ends up as a regular file, however it is stored.
    pub fn is_file(self) -> bool {
        matches!(self, Kind::File | Kind::Sparse)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub mode: u32,
    pub mtime: u64,
    /// Offset of the entry's body in the layer's uncompressed tar stream.
    pub offset: u64,
    pub size: u64,
    /// The path as the layer spells it, before it is rooted at the rootfs.
    pub path: Vec<u8>,
    /// Target of a symlink or hard link, empty otherwise.
    pub link: Vec<u8>,
    /// Names of the extended attributes the entry carries, NUL separated and
    /// empty for almost every entry. The values are not kept: nothing restores
    /// them, and this is only here so that every route can say so.
    pub xattrs: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub entries: Vec<Entry>,
}

impl Table {
    /// Walks an uncompressed tar stream and records what it holds.
    pub fn build(tar: impl Read) -> Result<Self> {
        let context = "reading a layer's entries";
        let mut archive = tar::Archive::new(tar);
        let mut entries = Vec::new();
        for entry in archive.entries().io_context(|| context.to_string())? {
            let mut entry = entry.io_context(|| context.to_string())?;
            let xattrs = xattr_names(&mut entry).io_context(|| context.to_string())?;
            let header = entry.header();
            let entry_type = header.entry_type();
            // Classified by what the extractor does with it, not by the type
            // alone: a continuous file is a plain file, while a sparse one
            // ends up as a file but is not stored as one.
            let kind = if entry_type.is_dir() {
                Kind::Directory
            } else if entry_type.is_symlink() {
                Kind::Symlink
            } else if entry_type.is_hard_link() {
                Kind::HardLink
            } else if entry_type == tar::EntryType::GNUSparse {
                Kind::Sparse
            } else if matches!(
                entry_type,
                tar::EntryType::Regular | tar::EntryType::Continuous
            ) {
                Kind::File
            } else {
                Kind::Unsupported
            };
            if entries.len() as u32 == MAX_ENTRIES {
                return Err(Error::io(context, io::Error::other("too many entries")));
            }
            entries.push(Entry {
                kind,
                mode: header.mode().unwrap_or(0o755) & 0o7777,
                mtime: header.mtime().unwrap_or(0),
                offset: entry.raw_file_position(),
                size: entry.size(),
                path: without_trailing_slash(entry.path_bytes().into_owned()),
                link: entry
                    .link_name_bytes()
                    .map(|link| link.into_owned())
                    .unwrap_or_default(),
                xattrs,
            });
        }
        Ok(Table { entries })
    }

    /// Paths dominate the table and repeat heavily between entries, so the
    /// whole of it goes through deflate: seven megabytes of chromium's entries
    /// become one, and the sidecar ships in a runfiles tree.
    pub fn write_to(&self, mut writer: impl Write) -> io::Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for entry in &self.entries {
            body.push(entry.kind.code());
            body.extend_from_slice(&entry.mode.to_le_bytes());
            body.extend_from_slice(&entry.mtime.to_le_bytes());
            body.extend_from_slice(&entry.offset.to_le_bytes());
            body.extend_from_slice(&entry.size.to_le_bytes());
            write_bytes(&mut body, &entry.path)?;
            write_bytes(&mut body, &entry.link)?;
            write_bytes(&mut body, &entry.xattrs)?;
        }

        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&body)?;
        let compressed = encoder.finish()?;

        writer.write_all(MAGIC)?;
        writer.write_all(&(body.len() as u64).to_le_bytes())?;
        writer.write_all(&(compressed.len() as u64).to_le_bytes())?;
        writer.write_all(&compressed)
    }

    pub fn read_from(mut reader: impl Read) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if magic != *MAGIC {
            return Err(io::Error::other("not an entry table"));
        }
        let plain_len = read_u64(&mut reader)?;
        let compressed_len = read_u64(&mut reader)?;
        // The length is what the reader trusts to size its buffer, so it is
        // checked before anything is allocated from it.
        if plain_len > (MAX_ENTRIES as u64) * 512 {
            return Err(io::Error::other("implausible entry table"));
        }
        let mut body = Vec::with_capacity(plain_len as usize);
        flate2::read::DeflateDecoder::new(reader.by_ref().take(compressed_len))
            .take(plain_len + 1)
            .read_to_end(&mut body)?;
        if body.len() as u64 != plain_len {
            return Err(io::Error::other("truncated entry table"));
        }

        let mut at = 0;
        let count = take_u32(&body, &mut at)?;
        if count > MAX_ENTRIES {
            return Err(io::Error::other("implausible entry count"));
        }
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let kind = Kind::from_code(take_u8(&body, &mut at)?)
                .ok_or_else(|| io::Error::other("unknown entry kind"))?;
            let mode = take_u32(&body, &mut at)?;
            let mtime = take_u64(&body, &mut at)?;
            let offset = take_u64(&body, &mut at)?;
            let size = take_u64(&body, &mut at)?;
            let path = take_bytes(&body, &mut at)?;
            let link = take_bytes(&body, &mut at)?;
            let xattrs = take_bytes(&body, &mut at)?;
            entries.push(Entry {
                kind,
                mode,
                mtime,
                offset,
                size,
                path,
                link,
                xattrs,
            });
        }
        if at != body.len() {
            return Err(io::Error::other("trailing bytes in entry table"));
        }
        Ok(Table { entries })
    }
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let len = u16::try_from(bytes.len())
        .map_err(|_| io::Error::other("path or link name is too long to record"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// The names of the entry's extended attributes, NUL separated.
///
/// Only the names: the values can be large, nothing restores them, and the
/// names are all a reader needs to say what an image asked for and did not get.
pub fn xattr_names(entry: &mut tar::Entry<'_, impl Read>) -> io::Result<Vec<u8>> {
    let Some(extensions) = entry.pax_extensions()? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for extension in extensions {
        let extension = extension?;
        let Ok(key) = extension.key() else { continue };
        let Some(name) = key.strip_prefix(XATTR_PREFIX) else {
            continue;
        };
        if !names.is_empty() {
            names.push(0);
        }
        names.extend_from_slice(name.as_bytes());
    }
    Ok(names)
}

/// `tar` spells a directory with a trailing slash, which would leave the plan/// holding `d/` for the directory and `d` for the same place as an ancestor of
/// what is under it. Worse, `d/` sorts inside its own subtree, so clearing the
/// subtree takes the directory with it.
fn without_trailing_slash(mut path: Vec<u8>) -> Vec<u8> {
    let kept = path.iter().rposition(|&byte| byte != b'/').map_or(0, |at| at + 1);
    // A path of nothing but slashes names the root, which has no shorter form.
    if kept > 0 {
        path.truncate(kept);
    }
    path
}

fn short(what: &str) -> io::Error {
    io::Error::other(format!("entry table ends inside {what}"))
}

fn take_u8(body: &[u8], at: &mut usize) -> io::Result<u8> {
    let byte = *body.get(*at).ok_or_else(|| short("an entry"))?;
    *at += 1;
    Ok(byte)
}

fn take_u32(body: &[u8], at: &mut usize) -> io::Result<u32> {
    let bytes = body
        .get(*at..*at + 4)
        .ok_or_else(|| short("an entry"))?
        .try_into()
        .expect("four bytes");
    *at += 4;
    Ok(u32::from_le_bytes(bytes))
}

fn take_u64(body: &[u8], at: &mut usize) -> io::Result<u64> {
    let bytes = body
        .get(*at..*at + 8)
        .ok_or_else(|| short("an entry"))?
        .try_into()
        .expect("eight bytes");
    *at += 8;
    Ok(u64::from_le_bytes(bytes))
}

fn take_bytes(body: &[u8], at: &mut usize) -> io::Result<Vec<u8>> {
    let len = {
        let bytes = body
            .get(*at..*at + 2)
            .ok_or_else(|| short("a name"))?
            .try_into()
            .expect("two bytes");
        *at += 2;
        u16::from_le_bytes(bytes) as usize
    };
    let bytes = body.get(*at..*at + len).ok_or_else(|| short("a name"))?;
    *at += len;
    Ok(bytes.to_vec())
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder
            .append_data(&mut header, "dir/", io::empty())
            .expect("dir");

        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_mtime(1_234_567);
        header.set_size(5);
        builder
            .append_data(&mut header, "dir/file", &b"hello"[..])
            .expect("file");

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "link", "dir/file")
            .expect("symlink");

        builder.into_inner().expect("tar")
    }

    #[test]
    fn a_table_records_what_the_layer_holds() {
        let tar = sample();
        let table = Table::build(&tar[..]).expect("table");
        let kinds: Vec<Kind> = table.entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [Kind::Directory, Kind::File, Kind::Symlink],
            "every entry is recorded, in order"
        );

        let file = &table.entries[1];
        assert_eq!(file.path, b"dir/file");
        assert_eq!(file.size, 5);
        assert_eq!(file.mode, 0o644);
        assert_eq!(file.mtime, 1_234_567);
        assert_eq!(
            &tar[file.offset as usize..file.offset as usize + 5],
            b"hello",
            "the offset names the body in the uncompressed stream"
        );
        assert_eq!(table.entries[2].link, b"dir/file");
    }

    /// The plan keys its tree on these paths, and `d/` would sort inside its
    /// own subtree rather than at the head of it.
    #[test]
    fn directory_paths_are_recorded_without_their_trailing_slash() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder
            .append_data(&mut header, "d/", std::io::empty())
            .expect("dir");

        let tar = builder.into_inner().expect("tar");
        let table = Table::build(&tar[..]).expect("table");
        assert_eq!(table.entries[0].path, b"d");
    }

    #[test]
    fn a_path_of_nothing_but_slashes_is_left_alone() {
        assert_eq!(without_trailing_slash(b"d/".to_vec()), b"d");
        assert_eq!(without_trailing_slash(b"a/b//".to_vec()), b"a/b");
        assert_eq!(without_trailing_slash(b"./".to_vec()), b".");
        assert_eq!(without_trailing_slash(b"file".to_vec()), b"file");
        assert_eq!(
            without_trailing_slash(b"/".to_vec()),
            b"/",
            "the root has no shorter form to reduce to"
        );
    }

    /// The values are not kept, only the names, and only so that a reader can
    /// say what the image asked for.
    #[test]
    fn extended_attribute_names_are_recorded_and_survive_serialisation() {
        let capability = [1u8, 0, 0, 2, 0, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut builder = tar::Builder::new(Vec::new());
        pax_record(&mut builder, "plain", b"body", &[]);
        pax_record(
            &mut builder,
            "capable",
            b"body",
            &[("security.capability", &capability), ("user.note", b"hello")],
        );
        let tar = builder.into_inner().expect("tar");

        let table = Table::build(&tar[..]).expect("table");
        assert_eq!(table.entries[0].xattrs, b"");
        assert_eq!(
            table.entries[1].xattrs,
            b"security.capability\0user.note",
            "names in the order the layer lists them, NUL separated"
        );

        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");
        assert_eq!(Table::read_from(&bytes[..]).expect("read"), table);
    }

    /// Writes a PAX extended header ahead of the entry, which is the only way
    /// a layer can carry an extended attribute. `tar::Builder` cannot.
    fn pax_record(
        builder: &mut tar::Builder<Vec<u8>>,
        path: &str,
        body: &[u8],
        xattrs: &[(&str, &[u8])],
    ) {
        if !xattrs.is_empty() {
            let mut records = Vec::new();
            for (name, value) in xattrs {
                let field = format!("SCHILY.xattr.{name}=");
                let mut len = field.len() + value.len() + 2;
                loop {
                    let next = len.to_string().len() + 1 + field.len() + value.len() + 1;
                    if next == len {
                        break;
                    }
                    len = next;
                }
                records.extend_from_slice(format!("{len} {field}").as_bytes());
                records.extend_from_slice(value);
                records.push(b'\n');
            }
            let mut header = tar::Header::new_ustar();
            header.set_entry_type(tar::EntryType::XHeader);
            header.set_mode(0o644);
            header.set_size(records.len() as u64);
            header.set_cksum();
            builder.append(&header, &records[..]).expect("pax header");
        }
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(body.len() as u64);
        builder.append_data(&mut header, path, body).expect("entry");
    }

    #[test]
    fn a_table_survives_serialisation() {        let table = Table::build(&sample()[..]).expect("table");
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");
        assert_eq!(Table::read_from(&bytes[..]).expect("read"), table);
    }

    #[test]
    fn an_empty_table_survives_serialisation() {
        let table = Table::default();
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");
        assert_eq!(Table::read_from(&bytes[..]).expect("read"), table);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(Table::read_from(&b"not a table at all"[..]).is_err());

        let table = Table::build(&sample()[..]).expect("table");
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).expect("write");

        // Anything cut inside the header, or inside the deflate stream that
        // follows it, leaves the reader without a body of the length the
        // header promised.
        for truncate_to in [4, 12, 19, bytes.len() / 2] {
            assert!(
                Table::read_from(&bytes[..truncate_to]).is_err(),
                "a table cut to {truncate_to} bytes must be refused"
            );
        }

        let mut flipped = bytes.clone();
        flipped[24] ^= 0xff;
        assert!(
            Table::read_from(&flipped[..]).is_err(),
            "a corrupted deflate stream must be refused"
        );
    }

    /// Types the extractor will not place still take part in resolution, so
    /// they have to be in the table rather than dropped from it.
    #[test]
    fn unsupported_types_are_recorded_rather_than_skipped() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Fifo);
        header.set_mode(0o644);
        header.set_size(0);
        builder.append_data(&mut header, "pipe", io::empty()).expect("fifo");
        let tar = builder.into_inner().expect("tar");

        let table = Table::build(&tar[..]).expect("table");
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.entries[0].kind, Kind::Unsupported);
        assert_eq!(table.entries[0].path, b"pipe");
    }
}
