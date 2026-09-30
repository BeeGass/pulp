//! Zip/tar expansion with zip-slip and zip-bomb guards.

use std::io::{self, Cursor, Read};

use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;
use zip::read::ZipFile;

use crate::error::Error;
use crate::extract::ExtractOpts;

/// Hard cap on total uncompressed bytes copied out of one top-level archive,
/// nested archives included.
pub(crate) const MAX_ARCHIVE_UNCOMPRESSED: u64 = 512 * 1024 * 1024;

/// Hard cap on decompressed tar stream bytes read for one top-level archive,
/// nested archives included. Skipping a member of a `.tar.gz` still means
/// inflating it, so this bounds the time a small, highly compressed archive
/// can cost even when its members are too large to keep.
pub(crate) const MAX_ARCHIVE_SCAN: u64 = 2 * 1024 * 1024 * 1024;

/// What one top-level archive, and every archive nested in it, may still
/// yield. Shared down the nesting so a zip of zips cannot multiply the caps.
#[derive(Debug, Clone)]
pub struct ArchiveBudget {
    copy_left: u64,
    scan_left: u64,
    cut: bool,
}

impl Default for ArchiveBudget {
    fn default() -> Self {
        Self::new(MAX_ARCHIVE_UNCOMPRESSED, MAX_ARCHIVE_SCAN)
    }
}

impl ArchiveBudget {
    #[must_use]
    pub fn new(copy: u64, scan: u64) -> Self {
        Self {
            copy_left: copy,
            scan_left: scan,
            cut: false,
        }
    }

    /// Whether a cap stopped an expansion early, leaving members out.
    #[must_use]
    pub fn cut(&self) -> bool {
        self.cut
    }
}

/// What to take from an archive member, judged by its name before any of
/// its bytes are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Bytes,
    /// Its name and size, without reading or charging it to the budget.
    Size,
    /// Nothing: the member is left out.
    Nothing,
}

/// One member read out of an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Member {
    /// A regular file at or under the per-file cap, with its bytes.
    File { name: String, bytes: Vec<u8> },
    /// A regular file over the per-file cap: named and sized, never read.
    TooLarge { name: String, size: u64 },
    /// A regular file the caller wanted named and sized, but not read.
    Unread { name: String, size: u64 },
    /// A file that could not be read out: stored with a method this build
    /// lacks, encrypted, damaged, or sharing its data with another member.
    Unreadable {
        name: String,
        size: u64,
        reason: String,
    },
}

impl Member {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::File { name, .. }
            | Self::TooLarge { name, .. }
            | Self::Unread { name, .. }
            | Self::Unreadable { name, .. } => name,
        }
    }
}

/// Expand a zip into members in archive order.
///
/// Names with a `..` component, absolute paths, and Windows drive prefixes
/// are skipped, as are directories and links, and members `want` passes
/// over; the rest are judged by `want` before anything is read. Members over
/// [`ExtractOpts::max_file_size`] come back as [`Member::TooLarge`]. Members
/// that no longer fit `budget` are left out and mark it cut. A member that
/// cannot be read, including one whose data overlaps another's (the trick
/// behind small zips that inflate to gigabytes), comes back as
/// [`Member::Unreadable`] in its place.
pub fn expand_zip_members(
    bytes: &[u8],
    opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
    want: &dyn Fn(&str) -> Want,
) -> Result<Vec<Member>, Error> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(zip_error)?;
    let overlapping = overlapping_entries(&mut archive);
    let mut out = Vec::new();
    for (i, overlaps) in overlapping.into_iter().enumerate() {
        // The central directory names every entry, even one that cannot be
        // opened; a directory's name ends in a separator.
        let Some(raw_name) = archive.name_for_index(i) else {
            continue;
        };
        if raw_name.ends_with(['/', '\\']) || is_rejected_member_name(raw_name) {
            continue;
        }
        let name = normalize_member_name(raw_name);
        let wanted = want(&name);
        if wanted == Want::Nothing {
            continue;
        }
        if overlaps {
            let reason = "its data overlaps another member's".to_string();
            out.push(unreadable_zip_member(&mut archive, i, name, reason));
            continue;
        }
        let reason = match archive.by_index(i) {
            Ok(mut file) => {
                out.extend(zip_file_member(&mut file, name, wanted, opts, budget));
                continue;
            }
            Err(err) => err.to_string(),
        };
        out.push(unreadable_zip_member(&mut archive, i, name, reason));
    }
    Ok(out)
}

/// Take an opened zip entry as a member, or leave it out: a link, or a
/// member that no longer fits `budget`.
fn zip_file_member(
    file: &mut ZipFile<'_, Cursor<&[u8]>>,
    name: String,
    wanted: Want,
    opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
) -> Option<Member> {
    if !file.is_file() {
        return None;
    }
    let claimed = file.size();
    if claimed > opts.max_file_size {
        return Some(Member::TooLarge {
            name,
            size: claimed,
        });
    }
    if wanted == Want::Size {
        return Some(Member::Unread {
            name,
            size: claimed,
        });
    }
    // A member that does not fit is left out, but smaller ones after it may
    // still fit.
    if claimed > budget.copy_left {
        budget.cut = true;
        return None;
    }
    Some(match read_claimed(file, claimed) {
        Ok(data) => {
            budget.copy_left = budget.copy_left.saturating_sub(data.len() as u64);
            Member::File { name, bytes: data }
        }
        Err(err) => Member::Unreadable {
            name,
            size: claimed,
            reason: err.to_string(),
        },
    })
}

/// A note for the zip entry at `index`, sized from its headers when they
/// can be read.
fn unreadable_zip_member(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    index: usize,
    name: String,
    reason: String,
) -> Member {
    let size = archive.by_index_raw(index).map_or(0, |file| file.size());
    Member::Unreadable { name, size, reason }
}

/// For each zip entry, whether its local header and data overlap an earlier
/// entry's. Honest archives never share bytes between entries.
fn overlapping_entries(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Vec<bool> {
    let mut spans: Vec<(u64, u64, usize)> = Vec::new();
    for i in 0..archive.len() {
        let Ok(file) = archive.by_index(i) else {
            continue;
        };
        let start = file.header_start();
        let end = file
            .data_start()
            .unwrap_or(start)
            .saturating_add(file.compressed_size());
        spans.push((start, end.max(start.saturating_add(1)), i));
    }
    spans.sort_unstable();
    let mut overlapping = vec![false; archive.len()];
    let mut reach = 0u64;
    for (start, end, index) in spans {
        if start < reach {
            overlapping[index] = true;
        } else {
            reach = end;
        }
    }
    overlapping
}

/// Expand a tar (optionally gzip-compressed) into members.
///
/// Same name, `want`, size, and budget policy as [`expand_zip_members`]. A
/// stream that breaks after some members keeps those members and marks the
/// budget cut.
pub fn expand_tar_members(
    bytes: &[u8],
    opts: &ExtractOpts,
    gzip: bool,
    budget: &mut ArchiveBudget,
    want: &dyn Fn(&str) -> Want,
) -> Result<Vec<Member>, Error> {
    if gzip {
        let scan_left = budget.scan_left;
        let mut reader = ScanLimit {
            inner: GzDecoder::new(bytes),
            left: scan_left,
            tripped: false,
        };
        let members = collect_tar(Archive::new(&mut reader), opts, budget, want);
        budget.scan_left = reader.left;
        // A spent budget truncates the archive; it does not make it
        // unreadable, even before its first member.
        if reader.tripped {
            budget.cut = true;
            return Ok(members.unwrap_or_default());
        }
        members
    } else {
        collect_tar(Archive::new(bytes), opts, budget, want)
    }
}

fn collect_tar<R: Read>(
    mut archive: Archive<R>,
    opts: &ExtractOpts,
    budget: &mut ArchiveBudget,
    want: &dyn Fn(&str) -> Want,
) -> Result<Vec<Member>, Error> {
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(_) if !out.is_empty() => {
                budget.cut = true;
                break;
            }
            Err(err) => return Err(err.into()),
        };
        if !entry.header().entry_type().is_file() {
            continue;
        }
        // Work from the raw bytes, so names that differ only in bytes that
        // are not UTF-8 stay apart. Separators are normalized first, then
        // those bytes are escaped, so an escape's `\` is never a separator.
        let raw: Vec<u8> = entry
            .path_bytes()
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect();
        if is_rejected_member_name(&String::from_utf8_lossy(&raw)) {
            continue;
        }
        let name = crate::tree::escape_invalid_utf8(&raw).into_owned();
        let wanted = want(&name);
        if wanted == Want::Nothing {
            continue;
        }
        let claimed = entry.size();
        if claimed > opts.max_file_size {
            out.push(Member::TooLarge {
                name,
                size: claimed,
            });
            continue;
        }
        if wanted == Want::Size {
            out.push(Member::Unread {
                name,
                size: claimed,
            });
            continue;
        }
        // Skipping a member only reads past it, which the scan cap bounds.
        if claimed > budget.copy_left {
            budget.cut = true;
            continue;
        }
        let data = match read_claimed(&mut entry, claimed) {
            Ok(data) => data,
            Err(_) if !out.is_empty() => {
                budget.cut = true;
                break;
            }
            Err(err) => return Err(err),
        };
        budget.copy_left = budget.copy_left.saturating_sub(data.len() as u64);
        out.push(Member::File { name, bytes: data });
    }
    Ok(out)
}

/// Reader that fails once it has handed out `left` bytes.
struct ScanLimit<R> {
    inner: R,
    left: u64,
    /// Whether a read failed because `left` ran out.
    tripped: bool,
}

impl<R: Read> Read for ScanLimit<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            self.tripped = true;
            return Err(io::Error::other("archive scan budget spent"));
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.left = self.left.saturating_sub(n as u64);
        Ok(n)
    }
}

/// Expand a zip into `(relative name, bytes)` pairs in archive order.
///
/// Members over [`ExtractOpts::max_file_size`] are left out; see
/// [`expand_zip_members`] for the rest of the policy.
pub fn expand_zip(bytes: &[u8], opts: &ExtractOpts) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let members = expand_zip_members(bytes, opts, &mut ArchiveBudget::default(), &|_| Want::Bytes)?;
    Ok(files_only(members))
}

/// Expand a tar (optionally gzip-compressed) into `(relative name, bytes)`
/// pairs, leaving out members over [`ExtractOpts::max_file_size`].
pub fn expand_tar(
    bytes: &[u8],
    opts: &ExtractOpts,
    gzip: bool,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let members = expand_tar_members(bytes, opts, gzip, &mut ArchiveBudget::default(), &|_| {
        Want::Bytes
    })?;
    Ok(files_only(members))
}

fn files_only(members: Vec<Member>) -> Vec<(String, Vec<u8>)> {
    members
        .into_iter()
        .filter_map(|member| match member {
            Member::File { name, bytes } => Some((name, bytes)),
            _ => None,
        })
        .collect()
}

fn read_claimed<R: Read>(reader: &mut R, claimed: u64) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    reader.take(claimed).read_to_end(&mut data)?;
    Ok(data)
}

/// Normalize archive path separators to `/`.
pub(crate) fn normalize_member_name(name: &str) -> String {
    name.replace('\\', "/")
}

/// True when a member name is zip-slip, absolute, or a Windows drive path.
///
/// Only a `..` path component counts as zip-slip. Names such as
/// `notes..v2.txt` or `...` are ordinary files.
pub(crate) fn is_rejected_member_name(name: &str) -> bool {
    let name = normalize_member_name(name);
    if name.starts_with('/') {
        return true;
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    name.split('/').any(|part| part == "..")
}

fn zip_error(err: zip::result::ZipError) -> Error {
    Error::msg(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    use tar::Builder;
    use zip::CompressionMethod;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn opts() -> ExtractOpts {
        ExtractOpts {
            max_file_size: 8 * 1024 * 1024,
            notebook_outputs: false,
            source_mode: false,
        }
    }

    fn everything(_: &str) -> Want {
        Want::Bytes
    }

    fn write_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        write_zip_with(CompressionMethod::Stored, entries)
    }

    fn write_zip_with(method: CompressionMethod, entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(method);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// Offsets of the local header and the central directory entry that
    /// name `name` in `zip`.
    fn headers_of(zip: &[u8], name: &str) -> (usize, usize) {
        let hits: Vec<usize> = zip
            .windows(name.len())
            .enumerate()
            .filter(|(_, window)| *window == name.as_bytes())
            .map(|(at, _)| at)
            .collect();
        (hits[0] - 30, hits[hits.len() - 1] - 46)
    }

    /// Names of the members read out, and of those noted as unreadable.
    fn read_and_noted(members: &[Member]) -> (Vec<&str>, Vec<&str>) {
        let read = members
            .iter()
            .filter(|member| matches!(member, Member::File { .. }))
            .map(Member::name)
            .collect();
        let noted = members
            .iter()
            .filter(|member| matches!(member, Member::Unreadable { .. }))
            .map(Member::name)
            .collect();
        (read, noted)
    }

    fn write_tar(entries: &[(&str, &[u8])], gzip: bool) -> Vec<u8> {
        if gzip {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut builder = Builder::new(encoder);
            append_tar(&mut builder, entries);
            builder.into_inner().unwrap().finish().unwrap()
        } else {
            let mut builder = Builder::new(Vec::new());
            append_tar(&mut builder, entries);
            builder.into_inner().unwrap()
        }
    }

    fn append_tar<W: Write>(builder: &mut Builder<W>, entries: &[(&str, &[u8])]) {
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, *name, *data).unwrap();
        }
    }

    /// A zip whose `count` central directory entries all point at one local
    /// file of `size` bytes, the layout of an overlapping zip bomb.
    fn overlapping_zip(count: usize, size: usize) -> Vec<u8> {
        let data = vec![b'a'; size];
        let crc = crc32(&data);
        let name0 = b"m0.txt";
        let size32 = u32::try_from(size).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for v in [20u16, 0, 0, 0, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, size32, size32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&u16::try_from(name0.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name0);
        out.extend_from_slice(&data);
        let cd_start = out.len();
        for i in 0..count {
            let name = format!("m{i}.txt");
            out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            for v in [20u16, 20, 0, 0, 0, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            for v in [crc, size32, size32] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
            for v in [0u16, 0, 0, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&[0u8; 8]);
            out.extend_from_slice(name.as_bytes());
        }
        let cd_len = out.len() - cd_start;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        let n = u16::try_from(count).unwrap();
        for v in [0u16, 0, n, n] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&u32::try_from(cd_len).unwrap().to_le_bytes());
        out.extend_from_slice(&u32::try_from(cd_start).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn test_expand_zip_with_zip_slip_returns_skipped() {
        let bytes = write_zip(&[
            ("../evil.txt", b"pwned"),
            ("foo/../../etc/passwd", b"pwned"),
            ("/abs.txt", b"nope"),
            ("C:\\Windows\\x.txt", b"nope"),
            ("nested\\..\\x.txt", b"nope"),
            ("ok.txt", b"hello"),
            ("dir/nested.txt", b"inside"),
        ]);
        let out = expand_zip(&bytes, &opts()).unwrap();
        assert_eq!(
            out,
            vec![
                ("ok.txt".to_string(), b"hello".to_vec()),
                ("dir/nested.txt".to_string(), b"inside".to_vec()),
            ]
        );
    }

    #[test]
    fn test_expand_zip_with_dots_inside_names_returns_those_members() {
        let bytes = write_zip(&[
            ("notes..v2.txt", b"double dot"),
            ("...", b"three dots"),
            ("dir/..hidden", b"leading dots"),
            ("a/../b.txt", b"slip"),
        ]);
        let names: Vec<String> = expand_zip(&bytes, &opts())
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["notes..v2.txt", "...", "dir/..hidden"]);
    }

    #[test]
    fn test_expand_zip_with_backslash_path_returns_normalized_name() {
        let bytes = write_zip(&[("dir\\file.txt", b"abc")]);
        let out = expand_zip(&bytes, &opts()).unwrap();
        assert_eq!(out, vec![("dir/file.txt".to_string(), b"abc".to_vec())]);
    }

    #[test]
    fn test_expand_tar_with_regular_file_returns_member() {
        let bytes = write_tar(&[("a.txt", b"tar-hi"), ("sub/b.txt", b"nested")], false);
        let out = expand_tar(&bytes, &opts(), false).unwrap();
        assert_eq!(
            out,
            vec![
                ("a.txt".to_string(), b"tar-hi".to_vec()),
                ("sub/b.txt".to_string(), b"nested".to_vec()),
            ]
        );
    }

    #[test]
    fn test_expand_tar_with_gzip_true_returns_member() {
        let bytes = write_tar(&[("gz.txt", b"gzipped")], true);
        let out = expand_tar(&bytes, &opts(), true).unwrap();
        assert_eq!(out, vec![("gz.txt".to_string(), b"gzipped".to_vec())]);
    }

    #[test]
    fn test_expand_zip_with_member_over_max_file_size_returns_skipped() {
        let bytes = write_zip(&[("big.bin", b"12345"), ("ok.txt", b"ok")]);
        let small = ExtractOpts {
            max_file_size: 2,
            notebook_outputs: false,
            source_mode: false,
        };
        let out = expand_zip(&bytes, &small).unwrap();
        assert_eq!(out, vec![("ok.txt".to_string(), b"ok".to_vec())]);
    }

    #[test]
    fn test_expand_zip_members_with_member_over_max_file_size_returns_too_large() {
        let bytes = write_zip(&[("big.bin", b"12345"), ("ok.txt", b"ok")]);
        let small = ExtractOpts {
            max_file_size: 2,
            notebook_outputs: false,
            source_mode: false,
        };
        let mut budget = ArchiveBudget::default();
        let out = expand_zip_members(&bytes, &small, &mut budget, &everything).unwrap();
        assert_eq!(
            out,
            vec![
                Member::TooLarge {
                    name: "big.bin".into(),
                    size: 5
                },
                Member::File {
                    name: "ok.txt".into(),
                    bytes: b"ok".to_vec()
                },
            ]
        );
        assert!(!budget.cut());
    }

    #[test]
    fn test_expand_zip_members_with_overlapping_entries_reads_first_and_notes_the_rest() {
        let bytes = overlapping_zip(50, 4096);
        let mut budget = ArchiveBudget::default();
        let out = expand_zip_members(&bytes, &opts(), &mut budget, &everything).unwrap();
        let (read, noted) = read_and_noted(&out);
        assert_eq!(read, ["m0.txt"]);
        assert_eq!(noted.len(), 49);
        assert_eq!(noted[0], "m1.txt");
        match &out[1] {
            Member::Unreadable { reason, .. } => assert!(reason.contains("overlaps"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test_expand_zip_members_with_one_corrupt_member_keeps_the_others() {
        let mut bytes = write_zip_with(
            CompressionMethod::Deflated,
            &[
                ("a_good.txt", &[b'a'; 4000]),
                ("b_bad.txt", &[b'b'; 4000]),
                ("c_good.txt", &[b'c'; 4000]),
            ],
        );
        let (local, _) = headers_of(&bytes, "b_bad.txt");
        let extra = usize::from(u16::from_le_bytes([bytes[local + 28], bytes[local + 29]]));
        let data = local + 30 + "b_bad.txt".len() + extra;
        for byte in &mut bytes[data..data + 6] {
            *byte ^= 0xFF;
        }
        let mut budget = ArchiveBudget::default();
        let out = expand_zip_members(&bytes, &opts(), &mut budget, &everything).unwrap();
        let (read, noted) = read_and_noted(&out);
        assert_eq!(read, ["a_good.txt", "c_good.txt"]);
        assert_eq!(noted, ["b_bad.txt"]);
    }

    #[test]
    fn test_expand_zip_members_with_bzip2_member_reports_it() {
        let mut bytes = write_zip(&[
            ("deflated.txt", b"kept"),
            ("squeezed.txt", b"lost"),
            ("stored.txt", b"kept too"),
        ]);
        // Mark the member as bzip2 (method 12), which this build cannot read.
        let (local, central) = headers_of(&bytes, "squeezed.txt");
        for at in [local + 8, central + 10] {
            bytes[at..at + 2].copy_from_slice(&12u16.to_le_bytes());
        }
        let mut budget = ArchiveBudget::default();
        let out = expand_zip_members(&bytes, &opts(), &mut budget, &everything).unwrap();
        let (read, noted) = read_and_noted(&out);
        assert_eq!(read, ["deflated.txt", "stored.txt"]);
        assert_eq!(noted, ["squeezed.txt"]);
    }

    #[test]
    fn test_expand_zip_members_with_encrypted_and_damaged_members_notes_each() {
        let mut bytes = write_zip(&[
            ("locked.txt", b"secret"),
            ("fine.txt", b"fine"),
            ("broken.txt", b"broken"),
        ]);
        let (local, central) = headers_of(&bytes, "locked.txt");
        bytes[local + 6] |= 1;
        bytes[central + 8] |= 1;
        let (local, _) = headers_of(&bytes, "broken.txt");
        bytes[local..local + 4].copy_from_slice(b"XXXX");
        let mut budget = ArchiveBudget::default();
        let out = expand_zip_members(&bytes, &opts(), &mut budget, &everything).unwrap();
        let (read, noted) = read_and_noted(&out);
        assert_eq!(read, ["fine.txt"]);
        assert_eq!(noted, ["locked.txt", "broken.txt"]);
    }

    #[test]
    fn test_expand_members_with_unwanted_members_never_reads_or_charges_them() {
        let entries: &[(&str, &[u8])] = &[
            ("skip.bin", &[b's'; 100]),
            ("size.dat", &[b'd'; 100]),
            ("keep.txt", &[b'k'; 40]),
        ];
        let want = |name: &str| match name {
            "skip.bin" => Want::Nothing,
            "size.dat" => Want::Size,
            _ => Want::Bytes,
        };
        let expected = vec![
            Member::Unread {
                name: "size.dat".into(),
                size: 100,
            },
            Member::File {
                name: "keep.txt".into(),
                bytes: vec![b'k'; 40],
            },
        ];
        let zip = write_zip(entries);
        let mut budget = ArchiveBudget::new(50, MAX_ARCHIVE_SCAN);
        assert_eq!(
            expand_zip_members(&zip, &opts(), &mut budget, &want).unwrap(),
            expected
        );
        assert!(!budget.cut());
        for gzip in [false, true] {
            let tar = write_tar(entries, gzip);
            let mut budget = ArchiveBudget::new(50, MAX_ARCHIVE_SCAN);
            let out = expand_tar_members(&tar, &opts(), gzip, &mut budget, &want).unwrap();
            assert_eq!(out, expected, "gzip {gzip}");
            assert!(!budget.cut());
        }
    }

    #[test]
    fn test_expand_members_with_shared_budget_stops_across_archives() {
        let first = write_zip(&[("a.txt", &[b'a'; 60]), ("b.txt", &[b'b'; 60])]);
        let second = write_tar(&[("c.txt", &[b'c'; 60]), ("d.txt", &[b'd'; 60])], true);
        let mut budget = ArchiveBudget::new(150, MAX_ARCHIVE_SCAN);
        let one = expand_zip_members(&first, &opts(), &mut budget, &everything).unwrap();
        assert_eq!(one.len(), 2);
        assert!(!budget.cut());
        let two = expand_tar_members(&second, &opts(), true, &mut budget, &everything).unwrap();
        let names: Vec<&str> = two.iter().map(Member::name).collect();
        assert!(names.is_empty(), "{names:?}");
        assert!(budget.cut());
    }

    #[test]
    fn test_expand_zip_members_with_budget_short_for_one_member_keeps_smaller_later_members() {
        let bytes = write_zip(&[
            ("a.txt", &[b'a'; 40]),
            ("big.bin", &[b'b'; 100]),
            ("c.txt", &[b'c'; 40]),
        ]);
        let mut budget = ArchiveBudget::new(90, MAX_ARCHIVE_SCAN);
        let out = expand_zip_members(&bytes, &opts(), &mut budget, &everything).unwrap();
        let names: Vec<&str> = out.iter().map(Member::name).collect();
        assert_eq!(names, ["a.txt", "c.txt"]);
        assert!(budget.cut());
    }

    #[test]
    fn test_expand_tar_members_with_budget_short_for_one_member_keeps_smaller_later_members() {
        for gzip in [false, true] {
            let bytes = write_tar(
                &[
                    ("a.txt", &[b'a'; 40]),
                    ("big.bin", &[b'b'; 100]),
                    ("c.txt", &[b'c'; 40]),
                ],
                gzip,
            );
            let mut budget = ArchiveBudget::new(90, MAX_ARCHIVE_SCAN);
            let out = expand_tar_members(&bytes, &opts(), gzip, &mut budget, &everything).unwrap();
            let names: Vec<&str> = out.iter().map(Member::name).collect();
            assert_eq!(names, ["a.txt", "c.txt"], "gzip {gzip}");
            assert!(budget.cut());
        }
    }

    #[test]
    fn test_expand_tar_members_with_spent_scan_budget_marks_cut() {
        let bytes = write_tar(&[("a.txt", b"first")], true);
        let mut budget = ArchiveBudget::new(MAX_ARCHIVE_UNCOMPRESSED, 0);
        let out = expand_tar_members(&bytes, &opts(), true, &mut budget, &everything).unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert!(budget.cut());
    }

    #[test]
    fn test_expand_tar_members_with_scan_cap_keeps_members_before_it() {
        let bytes = write_tar(
            &[
                ("first.txt", b"kept"),
                ("huge.bin", &vec![0u8; 64 * 1024]),
                ("after.txt", b"never reached"),
            ],
            true,
        );
        let small = ExtractOpts {
            max_file_size: 1024,
            notebook_outputs: false,
            source_mode: false,
        };
        let mut budget = ArchiveBudget::new(MAX_ARCHIVE_UNCOMPRESSED, 16 * 1024);
        let out = expand_tar_members(&bytes, &small, true, &mut budget, &everything).unwrap();
        let names: Vec<&str> = out.iter().map(Member::name).collect();
        assert_eq!(names, ["first.txt", "huge.bin"]);
        assert!(budget.cut());
    }

    #[cfg(unix)]
    #[test]
    fn test_expand_tar_with_non_utf8_names_returns_distinct_names() {
        use std::os::unix::ffi::OsStrExt;
        let mut builder = Builder::new(Vec::new());
        for name in [&b"a\xff.txt"[..], &b"a\xfe.txt"[..], &b"\xffstart.txt"[..]] {
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            header
                .set_path(std::path::Path::new(std::ffi::OsStr::from_bytes(name)))
                .unwrap();
            header.set_cksum();
            builder.append(&header, &b"x\n"[..]).unwrap();
        }
        let bytes = builder.into_inner().unwrap();
        let names: Vec<String> = expand_tar(&bytes, &opts(), false)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a\\xff.txt", "a\\xfe.txt", "\\xffstart.txt"]);
    }

    #[test]
    fn test_is_rejected_member_name_with_parent_dir_returns_true() {
        assert!(is_rejected_member_name("../evil.txt"));
        assert!(is_rejected_member_name("foo/../../etc/passwd"));
        assert!(is_rejected_member_name("/etc/passwd"));
        assert!(is_rejected_member_name("\\\\server\\share"));
        assert!(is_rejected_member_name("C:\\Windows\\x"));
        assert!(is_rejected_member_name("d:abs"));
        assert!(is_rejected_member_name("dir\\..\\x"));
        assert!(!is_rejected_member_name("dir/file.txt"));
        assert!(!is_rejected_member_name("file.txt"));
        assert!(!is_rejected_member_name("notes..v2.txt"));
        assert!(!is_rejected_member_name("..."));
    }
}
