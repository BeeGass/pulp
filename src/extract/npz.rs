//! NumPy `.npy` / `.npz` extraction into LLM-readable text.

use std::io::{Cursor, Read};

use zip::ZipArchive;

use super::archive::{
    MAX_ARCHIVE_FILES, MAX_ARCHIVE_UNCOMPRESSED, is_rejected_member_name, normalize_member_name,
};
use crate::error::Error;

const NPY_MAGIC: &[u8] = b"\x93NUMPY";
const PREVIEW_MAX_BYTES: usize = 4096;
const PREVIEW_MAX_VALUES: usize = 64;
const PREVIEW_MAX_DIMS: usize = 32;

/// Parse a `.npy` file and emit dtype / shape metadata plus a small preview.
pub fn extract_npy(bytes: &[u8]) -> Result<String, Error> {
    let parsed = parse_npy(bytes)?;
    Ok(render_npy(&parsed))
}

/// Parse an `.npz` (zip of `.npy` members) into named sections, sorted by name.
///
/// Zip-slip members and directories are skipped. Non-npy file members are
/// noted rather than treated as opaque binary.
pub fn extract_npz(bytes: &[u8]) -> Result<String, Error> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(zip_error)?;
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        if members.len() >= MAX_ARCHIVE_FILES {
            break;
        }
        let file = match archive.by_index(i) {
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
        if claimed > MAX_ARCHIVE_UNCOMPRESSED
            || total.saturating_add(claimed) > MAX_ARCHIVE_UNCOMPRESSED
        {
            continue;
        }
        let mut data = Vec::new();
        file.take(claimed).read_to_end(&mut data)?;
        total = total.saturating_add(data.len() as u64);
        members.push((name, data));
    }
    members.sort_by(|a, b| a.0.cmp(&b.0));

    let mut sections = Vec::new();
    for (name, data) in members {
        let display = npy_display_name(&name);
        if display.ends_with(".npy") {
            let text = extract_npy(&data)?;
            sections.push(format!("## {display}\n{text}"));
        } else {
            sections.push(format!(
                "[skipped non-npy entry {name}, {} bytes]",
                data.len()
            ));
        }
    }
    Ok(sections.join("\n\n"))
}

/// Use the basename so a directory prefix on an `.npy` member is ignored.
fn npy_display_name(name: &str) -> &str {
    match name.rsplit_once('/') {
        Some((_, base)) => base,
        None => name,
    }
}

struct NpyArray {
    descr: String,
    fortran_order: bool,
    shape: Vec<usize>,
    data: Vec<u8>,
}

struct Dtype {
    little_endian: bool,
    kind: char,
    itemsize: usize,
}

fn parse_npy(bytes: &[u8]) -> Result<NpyArray, Error> {
    if bytes.len() < 8 || !bytes.starts_with(NPY_MAGIC) {
        return Err(Error::msg("not an npy file"));
    }
    let major = bytes[6];
    let header_len_size = if major == 1 { 2 } else { 4 };
    let header_len_start = 8;
    let header_start = header_len_start + header_len_size;
    if bytes.len() < header_start {
        return Err(Error::msg("npy file truncated"));
    }
    let header_len = if major == 1 {
        u16::from_le_bytes([bytes[8], bytes[9]]) as usize
    } else {
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize
    };
    let data_start = match header_start.checked_add(header_len) {
        Some(n) => n,
        None => return Err(Error::msg("npy header length overflow")),
    };
    let header_bytes = match bytes.get(header_start..data_start) {
        Some(h) => h,
        None => return Err(Error::msg("npy file truncated")),
    };
    let header = std::str::from_utf8(header_bytes)
        .map_err(|_| Error::msg("npy header is not utf-8"))?
        .trim_end_matches(['\n', '\r', ' ', '\0']);
    let (descr, fortran_order, shape) = parse_header_dict(header)?;
    let data = bytes[data_start..].to_vec();
    Ok(NpyArray {
        descr,
        fortran_order,
        shape,
        data,
    })
}

fn parse_header_dict(header: &str) -> Result<(String, bool, Vec<usize>), Error> {
    let descr = parse_descr_field(after_key(header, "descr")?)?;
    let fortran_order = parse_bool_field(after_key(header, "fortran_order")?)?;
    let shape = parse_shape_field(after_key(header, "shape")?)?;
    Ok((descr, fortran_order, shape))
}

fn after_key<'a>(header: &'a str, key: &str) -> Result<&'a str, Error> {
    let single = format!("'{key}'");
    let double = format!("\"{key}\"");
    let mut starts = Vec::new();
    if let Some(i) = header.find(single.as_str()) {
        starts.push(i + single.len());
    }
    if let Some(i) = header.find(double.as_str()) {
        starts.push(i + double.len());
    }
    let start = starts
        .into_iter()
        .min()
        .ok_or_else(|| Error::msg(format!("npy header missing '{key}'")))?;
    let rest = header[start..].trim_start();
    rest.strip_prefix(':')
        .map(str::trim_start)
        .ok_or_else(|| Error::msg(format!("npy header missing value for '{key}'")))
}

fn parse_descr_field(s: &str) -> Result<String, Error> {
    let s = s.trim_start();
    if s.starts_with('\'') || s.starts_with('"') {
        let (value, _) = parse_quoted(s)?;
        return Ok(value.to_string());
    }
    let (value, _) = take_until_top_comma(s)?;
    Ok(value.trim().to_string())
}

fn parse_bool_field(s: &str) -> Result<bool, Error> {
    let s = s.trim_start();
    if s.starts_with("True") || s.starts_with("true") {
        Ok(true)
    } else if s.starts_with("False") || s.starts_with("false") {
        Ok(false)
    } else {
        Err(Error::msg("npy fortran_order is not a boolean"))
    }
}

fn parse_shape_field(s: &str) -> Result<Vec<usize>, Error> {
    let s = s.trim_start();
    let rest = s
        .strip_prefix('(')
        .ok_or_else(|| Error::msg("npy shape is not a tuple"))?;
    let mut dims = Vec::new();
    let mut cur = rest;
    loop {
        cur = cur.trim_start();
        if cur.starts_with(')') {
            break;
        }
        if cur.starts_with(',') {
            cur = cur[1..].trim_start();
            if cur.starts_with(')') {
                break;
            }
            continue;
        }
        let digit_end = cur.find(|c: char| !c.is_ascii_digit()).unwrap_or(cur.len());
        if digit_end == 0 {
            return Err(Error::msg("npy shape has an invalid dimension"));
        }
        let n: usize = cur[..digit_end]
            .parse()
            .map_err(|_| Error::msg("npy shape dimension overflow"))?;
        dims.push(n);
        cur = &cur[digit_end..];
        cur = cur.trim_start_matches(['L', 'l']);
    }
    Ok(dims)
}

fn parse_quoted(s: &str) -> Result<(&str, &str), Error> {
    let quote = match s.chars().next() {
        Some(c) if c == '\'' || c == '"' => c,
        _ => return Err(Error::msg("expected quoted string in npy header")),
    };
    let rest = &s[quote.len_utf8()..];
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == quote {
            return Ok((&rest[..i], &rest[i + quote.len_utf8()..]));
        }
    }
    Err(Error::msg("unterminated string in npy header"))
}

fn take_until_top_comma(s: &str) -> Result<(&str, &str), Error> {
    let mut depth_sq = 0i32;
    let mut depth_par = 0i32;
    let mut depth_br = 0i32;
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
                continue;
            }
            if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => in_str = Some(c),
            '[' => depth_sq += 1,
            ']' => depth_sq -= 1,
            '(' => depth_par += 1,
            ')' => depth_par -= 1,
            '{' => depth_br += 1,
            '}' => {
                depth_br -= 1;
                if depth_sq == 0 && depth_par == 0 && depth_br < 0 {
                    return Ok((s[..i].trim_end(), &s[i..]));
                }
            }
            ',' if depth_sq == 0 && depth_par == 0 && depth_br == 0 => {
                return Ok((s[..i].trim_end(), &s[i..]));
            }
            _ => {}
        }
    }
    Ok((s.trim_end(), ""))
}

fn parse_dtype(descr: &str) -> Option<Dtype> {
    let d = descr.trim();
    if d.is_empty() || d.starts_with(['[', '{', '(']) {
        return None;
    }
    let (little_endian, rest) = match d.as_bytes().first().copied() {
        Some(b'<') => (true, &d[1..]),
        Some(b'>') => (false, &d[1..]),
        Some(b'|' | b'=') => (true, &d[1..]),
        Some(_) => (true, d),
        None => return None,
    };
    let mut chars = rest.chars();
    let kind = chars.next()?;
    if !kind.is_ascii_alphabetic() {
        return None;
    }
    let size_str = chars.as_str();
    if size_str.is_empty() {
        return None;
    }
    let itemsize: usize = size_str.parse().ok()?;
    if itemsize == 0 {
        return None;
    }
    Some(Dtype {
        little_endian,
        kind,
        itemsize,
    })
}

fn is_numeric_dtype(dt: &Dtype) -> bool {
    matches!(dt.kind, 'i' | 'u' | 'f') && matches!(dt.itemsize, 1 | 2 | 4 | 8)
}

fn render_npy(parsed: &NpyArray) -> String {
    let elements = parsed
        .shape
        .iter()
        .try_fold(1usize, |acc, &dim| acc.checked_mul(dim));
    let mut lines = Vec::new();
    lines.push("format: npy".to_string());
    lines.push(format!("descr: {}", parsed.descr));
    lines.push(format!(
        "fortran_order: {}",
        if parsed.fortran_order {
            "true"
        } else {
            "false"
        }
    ));
    lines.push(format!("shape: {}", format_shape(&parsed.shape)));
    match elements {
        Some(n) => lines.push(format!("elements: {n}")),
        None => lines.push("elements: overflow".to_string()),
    }
    lines.push(preview_line(parsed, elements));
    lines.join("\n")
}

fn format_shape(shape: &[usize]) -> String {
    match shape.len() {
        0 => "()".to_string(),
        1 => format!("({},)", shape[0]),
        _ => {
            let mut s = String::from("(");
            for (i, dim) in shape.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&dim.to_string());
            }
            s.push(')');
            s
        }
    }
}

fn preview_line(parsed: &NpyArray, elements: Option<usize>) -> String {
    let omitted = "preview: omitted (too large or unsupported dtype)";
    let (Some(n_elem), Some(dt)) = (elements, parse_dtype(&parsed.descr)) else {
        return omitted.to_string();
    };
    if !is_numeric_dtype(&dt) || parsed.shape.len() > PREVIEW_MAX_DIMS {
        return omitted.to_string();
    }
    let Some(n_bytes) = n_elem.checked_mul(dt.itemsize) else {
        return omitted.to_string();
    };
    if n_bytes > PREVIEW_MAX_BYTES || parsed.data.len() < n_bytes {
        return omitted.to_string();
    }
    let payload = &parsed.data[..n_bytes];
    let ordered = c_order_bytes(payload, &parsed.shape, dt.itemsize, parsed.fortran_order);
    match decode_numeric(&ordered, &dt) {
        Ok(values) => {
            let body = format_nested(&values, &parsed.shape, PREVIEW_MAX_VALUES);
            format!("preview: {body}")
        }
        Err(_) => omitted.to_string(),
    }
}

fn c_order_bytes(data: &[u8], shape: &[usize], itemsize: usize, fortran: bool) -> Vec<u8> {
    if !fortran || shape.len() <= 1 {
        return data.to_vec();
    }
    let mut out = vec![0u8; data.len()];
    let n = data.len() / itemsize;
    for c_index in 0..n {
        let f_index = c_index_to_f_index(c_index, shape);
        let src = f_index * itemsize;
        let dst = c_index * itemsize;
        if src + itemsize <= data.len() && dst + itemsize <= out.len() {
            out[dst..dst + itemsize].copy_from_slice(&data[src..src + itemsize]);
        }
    }
    out
}

fn c_index_to_f_index(mut c_index: usize, shape: &[usize]) -> usize {
    let mut coords = vec![0usize; shape.len()];
    for i in (0..shape.len()).rev() {
        let dim = shape[i];
        if dim == 0 {
            return 0;
        }
        coords[i] = c_index % dim;
        c_index /= dim;
    }
    let mut stride = 1usize;
    let mut f_index = 0usize;
    for i in 0..shape.len() {
        f_index += coords[i] * stride;
        stride *= shape[i];
    }
    f_index
}

fn decode_numeric(data: &[u8], dt: &Dtype) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for chunk in data.chunks_exact(dt.itemsize) {
        out.push(decode_one(chunk, dt)?);
    }
    Ok(out)
}

fn decode_one(chunk: &[u8], dt: &Dtype) -> Result<String, Error> {
    let le = dt.little_endian;
    match (dt.kind, dt.itemsize) {
        ('i', 1) => Ok((chunk[0] as i8).to_string()),
        ('u', 1) => Ok(chunk[0].to_string()),
        ('i', 2) => Ok(i16_from(chunk, le)?.to_string()),
        ('u', 2) => Ok(u16_from(chunk, le)?.to_string()),
        ('i', 4) => Ok(i32_from(chunk, le)?.to_string()),
        ('u', 4) => Ok(u32_from(chunk, le)?.to_string()),
        ('i', 8) => Ok(i64_from(chunk, le)?.to_string()),
        ('u', 8) => Ok(u64_from(chunk, le)?.to_string()),
        ('f', 2) => {
            let bits = u16_from(chunk, le)?;
            Ok(format!("{:?}", f16_to_f32(bits)))
        }
        ('f', 4) => Ok(format!("{:?}", f32_from(chunk, le)?)),
        ('f', 8) => Ok(format!("{:?}", f64_from(chunk, le)?)),
        _ => Err(Error::msg("unsupported npy dtype for preview")),
    }
}

fn array_from<const N: usize>(chunk: &[u8]) -> Result<[u8; N], Error> {
    chunk
        .try_into()
        .map_err(|_| Error::msg("npy value truncated"))
}

fn i16_from(chunk: &[u8], le: bool) -> Result<i16, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        i16::from_le_bytes(b)
    } else {
        i16::from_be_bytes(b)
    })
}

fn u16_from(chunk: &[u8], le: bool) -> Result<u16, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        u16::from_le_bytes(b)
    } else {
        u16::from_be_bytes(b)
    })
}

fn i32_from(chunk: &[u8], le: bool) -> Result<i32, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    })
}

fn u32_from(chunk: &[u8], le: bool) -> Result<u32, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    })
}

fn i64_from(chunk: &[u8], le: bool) -> Result<i64, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        i64::from_le_bytes(b)
    } else {
        i64::from_be_bytes(b)
    })
}

fn u64_from(chunk: &[u8], le: bool) -> Result<u64, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        u64::from_le_bytes(b)
    } else {
        u64::from_be_bytes(b)
    })
}

fn f32_from(chunk: &[u8], le: bool) -> Result<f32, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        f32::from_le_bytes(b)
    } else {
        f32::from_be_bytes(b)
    })
}

fn f64_from(chunk: &[u8], le: bool) -> Result<f64, Error> {
    let b = array_from(chunk)?;
    Ok(if le {
        f64::from_le_bytes(b)
    } else {
        f64::from_be_bytes(b)
    })
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from((bits >> 15) & 1);
    let exp = u32::from((bits >> 10) & 0x1f);
    let frac = u32::from(bits & 0x3ff);
    let out = if exp == 0 {
        if frac == 0 {
            sign << 31
        } else {
            let mut e: i32 = 127 - 15 + 1;
            let mut m = frac;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3ff;
            (sign << 31) | ((e as u32) << 23) | (m << 13)
        }
    } else if exp == 31 {
        (sign << 31) | (0xff << 23) | (frac << 13)
    } else {
        let e = exp - 15 + 127;
        (sign << 31) | (e << 23) | (frac << 13)
    };
    f32::from_bits(out)
}

fn format_nested(values: &[String], shape: &[usize], max_values: usize) -> String {
    if shape.is_empty() {
        return values.first().cloned().unwrap_or_else(|| "[]".to_string());
    }
    let mut idx = 0usize;
    format_level(values, shape, &mut idx, max_values)
}

fn format_level(values: &[String], shape: &[usize], idx: &mut usize, max_values: usize) -> String {
    if shape.is_empty() {
        if *idx >= values.len() {
            return "[]".to_string();
        }
        let v = values[*idx].clone();
        *idx += 1;
        return v;
    }
    let dim = shape[0];
    let rest = &shape[1..];
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..dim {
        if *idx >= max_values || *idx >= values.len() {
            break;
        }
        parts.push(format_level(values, rest, idx, max_values));
    }
    if parts.len() < dim {
        parts.push("...".to_string());
    }
    format!("[{}]", parts.join(", "))
}

fn zip_error(err: zip::result::ZipError) -> Error {
    Error::msg(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    use zip::CompressionMethod;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn format_shape_for_header(shape: &[usize]) -> String {
        match shape.len() {
            0 => "()".to_string(),
            1 => format!("({},)", shape[0]),
            _ => {
                let inner = shape
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inner})")
            }
        }
    }

    fn build_npy_v1(descr: &str, fortran_order: bool, shape: &[usize], payload: &[u8]) -> Vec<u8> {
        let fo = if fortran_order { "True" } else { "False" };
        let dict = format!(
            "{{'descr': '{descr}', 'fortran_order': {fo}, 'shape': {}, }}",
            format_shape_for_header(shape)
        );
        let mut header = dict;
        let mut total = 10 + header.len() + 1;
        let misalign = total % 64;
        if misalign != 0 {
            header.push_str(&" ".repeat(64 - misalign));
            total = 10 + header.len() + 1;
        }
        header.push('\n');
        assert_eq!(10 + header.len(), total);
        let header_len = u16::try_from(header.len()).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(NPY_MAGIC);
        out.extend_from_slice(&[1, 0]);
        out.extend_from_slice(&header_len.to_le_bytes());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn f32_payload(values: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(values.len() * 4);
        for v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
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

    #[test]
    fn test_extract_npy_with_tiny_float32_vector_returns_preview() {
        let payload = f32_payload(&[1.0, 2.0, 3.0]);
        let bytes = build_npy_v1("<f4", false, &[3], &payload);
        let text = extract_npy(&bytes).unwrap();
        assert!(text.contains("format: npy"));
        assert!(text.contains("descr: <f4"));
        assert!(text.contains("fortran_order: false"));
        assert!(text.contains("shape: (3,)"));
        assert!(text.contains("elements: 3"));
        assert!(text.contains("preview: [1.0, 2.0, 3.0]"));
    }

    #[test]
    fn test_extract_npy_with_2d_float32_returns_nested_preview() {
        let payload = f32_payload(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let bytes = build_npy_v1("<f4", false, &[2, 3], &payload);
        let text = extract_npy(&bytes).unwrap();
        assert!(text.contains("shape: (2, 3)"));
        assert!(text.contains("elements: 6"));
        assert!(text.contains("preview: [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]"));
    }

    #[test]
    fn test_extract_npy_with_more_than_64_values_returns_capped_preview() {
        let values: Vec<f32> = (0..70).map(|i| i as f32).collect();
        let payload = f32_payload(&values);
        let bytes = build_npy_v1("<f4", false, &[70], &payload);
        let text = extract_npy(&bytes).unwrap();
        assert!(text.contains("elements: 70"));
        assert!(text.contains("63.0, ..."));
        assert!(!text.contains("64.0"));
    }

    #[test]
    fn test_extract_npy_with_unsupported_dtype_returns_omitted_preview() {
        let bytes = build_npy_v1("|S3", false, &[1], b"abc");
        let text = extract_npy(&bytes).unwrap();
        assert!(text.contains("descr: |S3"));
        assert!(text.contains("preview: omitted (too large or unsupported dtype)"));
    }

    #[test]
    fn test_extract_npz_with_one_small_npy_returns_named_section() {
        let payload = f32_payload(&[1.0, 2.0]);
        let npy = build_npy_v1("<f4", false, &[2], &payload);
        let zip = write_zip(&[("arr_0.npy", npy.as_slice()), ("readme.txt", b"hi")]);
        let text = extract_npz(&zip).unwrap();
        assert!(text.contains("## arr_0.npy"));
        assert!(text.contains("descr: <f4"));
        assert!(text.contains("preview: [1.0, 2.0]"));
        assert!(text.contains("[skipped non-npy entry readme.txt, 2 bytes]"));
    }

    #[test]
    fn test_extract_npz_with_zip_slip_member_returns_skipped() {
        let payload = f32_payload(&[1.0]);
        let npy = build_npy_v1("<f4", false, &[1], &payload);
        let zip = write_zip(&[("../evil.npy", npy.as_slice()), ("ok.npy", npy.as_slice())]);
        let text = extract_npz(&zip).unwrap();
        assert!(text.contains("## ok.npy"));
        assert!(!text.contains("evil"));
    }

    #[test]
    fn test_extract_npy_with_missing_magic_returns_error() {
        let err = extract_npy(b"not npy").unwrap_err();
        assert!(err.to_string().contains("not an npy file"));
    }
}
