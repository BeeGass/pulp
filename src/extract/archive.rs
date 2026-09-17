//! Zip/tar expansion with zip-slip and zip-bomb guards.

use std::io::{Cursor, Read};

use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

use crate::error::Error;
use crate::extract::ExtractOpts;

/// Hard cap on file members copied from one archive.
pub(crate) const MAX_ARCHIVE_FILES: usize = 10_000;
/// Hard cap on total uncompressed bytes copied from one archive.
pub(crate) const MAX_ARCHIVE_UNCOMPRESSED: u64 = 512 * 1024 * 1024;

/// Expand a zip into `(relative name, bytes)` pairs in archive order.
///
/// Names with `..`, absolute paths, and Windows drive prefixes are skipped.
/// Directories are skipped. Members larger than [`ExtractOpts::max_file_size`]
/// or that would exceed 512 MiB total uncompressed are skipped.
pub fn expand_zip(bytes: &[u8], opts: &ExtractOpts) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(zip_error)?;
    let mut out = Vec::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        if out.len() >= MAX_ARCHIVE_FILES {
            break;
        }
        let mut file = match archive.by_index(i) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if !file.is_file() {
            continue;
        }
        let raw_name = file.name();
        if is_rejected_member_name(raw_name) {
            continue;
        }
        let name = normalize_member_name(raw_name);
        let claimed = file.size();
        if !can_copy_member(claimed, opts, total) {
            continue;
        }
        let data = read_claimed(&mut file, claimed)?;
        total = total.saturating_add(data.len() as u64);
        out.push((name, data));
    }
    Ok(out)
}

/// Expand a tar (optionally gzip-compressed) into `(relative name, bytes)` pairs.
///
/// Same zip-slip / size policy as [`expand_zip`].
pub fn expand_tar(
    bytes: &[u8],
    opts: &ExtractOpts,
    gzip: bool,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    if gzip {
        collect_tar(Archive::new(GzDecoder::new(bytes)), opts)
    } else {
        collect_tar(Archive::new(bytes), opts)
    }
}

fn collect_tar<R: Read>(
    mut archive: Archive<R>,
    opts: &ExtractOpts,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut out = Vec::new();
    let mut total = 0u64;
    for entry in archive.entries()? {
        if out.len() >= MAX_ARCHIVE_FILES {
            break;
        }
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let raw_name = match entry.path() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        if is_rejected_member_name(&raw_name) {
            continue;
        }
        let name = normalize_member_name(&raw_name);
        let claimed = entry.size();
        if !can_copy_member(claimed, opts, total) {
            continue;
        }
        let data = read_claimed(&mut entry, claimed)?;
        total = total.saturating_add(data.len() as u64);
        out.push((name, data));
    }
    Ok(out)
}

fn can_copy_member(claimed: u64, opts: &ExtractOpts, total: u64) -> bool {
    claimed <= opts.max_file_size && total.saturating_add(claimed) <= MAX_ARCHIVE_UNCOMPRESSED
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
pub(crate) fn is_rejected_member_name(name: &str) -> bool {
    let name = normalize_member_name(name);
    if name.starts_with('/') {
        return true;
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    name.contains("..")
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

    fn write_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
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
    fn test_is_rejected_member_name_with_parent_dir_returns_true() {
        assert!(is_rejected_member_name("../evil.txt"));
        assert!(is_rejected_member_name("foo/../../etc/passwd"));
        assert!(is_rejected_member_name("/etc/passwd"));
        assert!(is_rejected_member_name("\\\\server\\share"));
        assert!(is_rejected_member_name("C:\\Windows\\x"));
        assert!(is_rejected_member_name("d:abs"));
        assert!(!is_rejected_member_name("dir/file.txt"));
        assert!(!is_rejected_member_name("file.txt"));
    }
}
