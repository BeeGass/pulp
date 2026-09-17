use std::path::Path;

/// Detected input kind. Extension wins over magic bytes so `.ts` is
/// TypeScript, not MPEG-TS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
    Xml,
    Json,
    Csv,
    Tsv,
    Pdf,
    Docx,
    Pptx,
    Spreadsheet,
    Odt,
    Odp,
    Epub,
    Rtf,
    Notebook,
    Npy,
    Npz,
    Zip,
    Tar,
    TarGz,
    Binary,
    Unknown,
}

impl Kind {
    #[must_use]
    pub fn is_archive(self) -> bool {
        matches!(self, Self::Zip | Self::Tar | Self::TarGz)
    }

    #[must_use]
    pub fn is_binary_media(self) -> bool {
        matches!(self, Self::Binary)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Xml => "xml",
            Self::Json => "json",
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Spreadsheet => "sheet",
            Self::Odt => "odt",
            Self::Odp => "odp",
            Self::Epub => "epub",
            Self::Rtf => "rtf",
            Self::Notebook => "ipynb",
            Self::Npy => "npy",
            Self::Npz => "npz",
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "targz",
            Self::Binary => "binary",
            Self::Unknown => "unknown",
        }
    }
}

/// True when the first 8 KiB contain a NUL, which almost never happens
/// in text we want to dump as source.
#[must_use]
pub fn looks_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(8192);
    bytes[..n].contains(&0)
}

#[must_use]
pub fn classify(path: &Path, sniff: Option<&[u8]>) -> Kind {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if is_known_text_filename(&name) {
        return Kind::Text;
    }

    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Kind::TarGz;
    }
    if lower.ends_with(".tar") {
        return Kind::Tar;
    }

    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(kind) = ext_kind(&ext.to_ascii_lowercase()) {
            return kind;
        }
    }

    if let Some(bytes) = sniff {
        if let Some(kind) = sniff_kind(bytes) {
            return kind;
        }
        if looks_binary(bytes) {
            return Kind::Binary;
        }
        return Kind::Text;
    }

    Kind::Unknown
}

/// Whether the mill should tick this file after a scan.
///
/// Lockfiles, raster/vector images, and other binary media stay in the
/// tree but start unchecked.
#[must_use]
pub fn is_default_selected(path: &Path, kind: Kind) -> bool {
    if kind.is_binary_media() {
        return false;
    }
    !is_lock_file(path) && !is_image_file(path)
}

fn is_lock_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".lock")
        || name.ends_with(".lockb")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name.ends_with("-lock.yml")
        || matches!(
            name.as_str(),
            "go.sum" | "npm-shrinkwrap.json" | "shrinkwrap.yaml"
        )
}

fn is_image_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "svg"
            | "svgz"
            | "png"
            | "jpg"
            | "jpeg"
            | "jpe"
            | "gif"
            | "webp"
            | "ico"
            | "icns"
            | "bmp"
            | "tif"
            | "tiff"
            | "heic"
            | "heif"
            | "avif"
            | "psd"
            | "ai"
            | "eps"
    )
}

fn is_known_text_filename(name: &str) -> bool {
    matches!(
        name,
        "dockerfile"
            | "makefile"
            | "gnumakefile"
            | "license"
            | "licence"
            | "copying"
            | "authors"
            | "contributors"
            | "changelog"
            | "readme"
            | "cargo.lock"
            | "gemfile"
            | "rakefile"
            | "procfile"
            | "justfile"
            | "cmakelists.txt"
            | "vagrantfile"
            | "lakefile.lean"
            | "lakefile.toml"
    )
}

fn ext_kind(ext: &str) -> Option<Kind> {
    Some(match ext {
        "html" | "htm" | "xhtml" => Kind::Html,
        "xml" | "xsl" | "xslt" | "plist" => Kind::Xml,
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" | "geojson" => Kind::Json,
        "csv" => Kind::Csv,
        "tsv" => Kind::Tsv,
        "pdf" => Kind::Pdf,
        "docx" | "dotx" => Kind::Docx,
        "pptx" | "potx" => Kind::Pptx,
        "xlsx" | "xlsm" | "xls" | "xltx" | "ods" => Kind::Spreadsheet,
        "odt" => Kind::Odt,
        "odp" => Kind::Odp,
        "epub" => Kind::Epub,
        "rtf" => Kind::Rtf,
        "ipynb" => Kind::Notebook,
        "npy" => Kind::Npy,
        "npz" => Kind::Npz,
        "zip" | "zipx" => Kind::Zip,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" | "tif" | "tiff" | "heic"
        | "avif" | "psd" | "woff" | "woff2" | "ttf" | "otf" | "eot" | "mp3" | "mp4" | "m4a"
        | "aac" | "ogg" | "flac" | "wav" | "avi" | "mov" | "mkv" | "webm" | "exe" | "dll"
        | "so" | "dylib" | "o" | "a" | "lib" | "class" | "pyc" | "pyo" | "rlib" | "rmeta"
        | "wasm" | "bin" | "dat" | "sqlite" | "sqlite3" | "db" | "parquet" | "arrow"
        | "feather" | "pkl" | "pickle" | "joblib" | "safetensors" | "pt" | "pth" | "onnx"
        | "gguf" | "ggml" | "binpb" | "icns" | "dmg" | "iso" | "img" | "pack" | "idx" | "pdb"
        | "ilk" | "obj" => Kind::Binary,
        "rs" | "lean" | "py" | "pyi" | "pyw" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx"
        | "go" | "java" | "kt" | "kts" | "c" | "h" | "hh" | "hpp" | "hxx" | "cpp" | "cc"
        | "cxx" | "m" | "mm" | "cs" | "fs" | "fsx" | "fsi" | "swift" | "rb" | "php" | "lua"
        | "r" | "jl" | "sql" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "cnf" | "sh"
        | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "md" | "markdown" | "mdx" | "rst"
        | "org" | "tex" | "bib" | "txt" | "text" | "log" | "ninja" | "cmake" | "mk" | "make"
        | "gradle" | "sbt" | "vim" | "el" | "lisp" | "clj" | "cljs" | "edn" | "hs" | "lhs"
        | "ml" | "mli" | "nim" | "zig" | "v" | "d" | "pas" | "pl" | "pm" | "raku" | "proto"
        | "thrift" | "graphql" | "gql" | "prisma" | "vue" | "svelte" | "astro" | "css" | "scss"
        | "less" | "sass" | "nix" | "erl" | "hrl" | "ex" | "exs" | "heex" | "dart" | "scala"
        | "sc" | "groovy" | "tf" | "hcl" | "cue" | "jsonnet" | "bzl" | "bazel" | "wat" | "wit"
        | "sol" | "move" | "wgsl" | "glsl" | "hlsl" | "metal" | "asm" | "s" | "diff" | "patch"
        | "gitignore" | "gitattributes" | "dockerignore" | "editorconfig" | "env"
        | "properties" | "csvmd" | "svg" | "dot" | "gv" | "puml" | "plantuml" | "lock" | "sum"
        | "mod" | "work" | "cabal" | "hsig" | "rake" | "gemspec" | "podspec" | "csproj"
        | "fsproj" | "vbproj" | "sln" | "dtd" | "xsd" | "wsdl" | "tomlrc" | "npmrc" | "nvmrc"
        | "prettierrc" | "eslintrc" | "babelrc" | "zshrc" | "bashrc" | "profile" | "gitconfig"
        | "hurl" | "http" | "rest" | "ron" | "kdl" | "hjson" | "cson" | "jade" | "pug" | "haml"
        | "slim" | "mustache" | "hbs" | "jinja" | "j2" | "njk" | "ejs" | "erb" | "twig"
        | "liquid" | "mjml" | "adoc" | "asciidoc" | "textile" | "wiki" | "pod" | "rdoc" | "1"
        | "2" | "3" | "man" | "mdoc" | "header" | "ipp" | "tpp" | "inl" | "inc" | "def"
        | "asmx" | "aspx" | "ascx" | "master" | "cshtml" | "razor" | "jsp" | "jspx" | "cfm"
        | "cfc" => Kind::Text,
        _ => return None,
    })
}

fn sniff_kind(bytes: &[u8]) -> Option<Kind> {
    if bytes.starts_with(b"%PDF") {
        return Some(Kind::Pdf);
    }
    if bytes.starts_with(br"{\rtf") {
        return Some(Kind::Rtf);
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Some(Kind::TarGz);
    }
    if let Some(info) = infer::get(bytes) {
        return Some(match info.mime_type() {
            "application/pdf" => Kind::Pdf,
            "application/epub+zip" => Kind::Epub,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Kind::Docx,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
                Kind::Pptx
            }
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-excel"
            | "application/vnd.oasis.opendocument.spreadsheet" => Kind::Spreadsheet,
            "application/vnd.oasis.opendocument.text" => Kind::Odt,
            "application/vnd.oasis.opendocument.presentation" => Kind::Odp,
            "application/zip" | "application/x-zip-compressed" => Kind::Zip,
            "application/x-tar" => Kind::Tar,
            "application/gzip" | "application/x-gzip" => Kind::TarGz,
            "text/html" => Kind::Html,
            "text/xml" | "application/xml" => Kind::Xml,
            "application/json" => Kind::Json,
            "text/csv" => Kind::Csv,
            mime if mime.starts_with("image/")
                || mime.starts_with("audio/")
                || mime.starts_with("video/")
                || mime.starts_with("font/")
                || mime == "application/octet-stream" =>
            {
                Kind::Binary
            }
            _ => return None,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_classify_with_rust_extension_returns_text() {
        assert_eq!(classify(Path::new("src/lib.rs"), None), Kind::Text);
    }

    #[test]
    fn test_classify_with_docx_extension_returns_docx() {
        assert_eq!(classify(Path::new("spec.docx"), None), Kind::Docx);
    }

    #[test]
    fn test_classify_with_pdf_magic_returns_pdf() {
        assert_eq!(
            classify(Path::new("unknown"), Some(b"%PDF-1.7\n")),
            Kind::Pdf
        );
    }

    #[test]
    fn test_classify_with_null_bytes_returns_binary() {
        assert_eq!(
            classify(Path::new("blob"), Some(&[b'a', 0, b'b'])),
            Kind::Binary
        );
    }

    #[test]
    fn test_classify_with_ts_extension_returns_text_not_video() {
        assert_eq!(
            classify(Path::new("app.ts"), Some(b"export const x = 1;\n")),
            Kind::Text
        );
    }

    #[test]
    fn test_classify_with_lean_extension_returns_text() {
        assert_eq!(classify(Path::new("Dual/Basic.lean"), None), Kind::Text);
    }

    #[test]
    fn test_classify_with_npz_extension_returns_npz() {
        assert_eq!(classify(Path::new("runs/params.npz"), None), Kind::Npz);
    }

    #[test]
    fn test_classify_with_npy_extension_returns_npy() {
        assert_eq!(classify(Path::new("cache/batch.npy"), None), Kind::Npy);
    }

    #[test]
    fn test_as_str_with_lean_and_npz_returns_labels() {
        assert_eq!(Kind::Text.as_str(), "text");
        assert_eq!(Kind::Npz.as_str(), "npz");
        assert_eq!(Kind::Npy.as_str(), "npy");
    }

    #[test]
    fn test_is_default_selected_with_lockfile_returns_false() {
        assert!(!is_default_selected(Path::new("Cargo.lock"), Kind::Text));
        assert!(!is_default_selected(
            Path::new("package-lock.json"),
            Kind::Json
        ));
        assert!(!is_default_selected(Path::new("yarn.lock"), Kind::Text));
        assert!(!is_default_selected(Path::new("go.sum"), Kind::Text));
    }

    #[test]
    fn test_is_default_selected_with_image_returns_false() {
        assert!(!is_default_selected(Path::new("logo.svg"), Kind::Text));
        assert!(!is_default_selected(Path::new("hero.png"), Kind::Binary));
        assert!(!is_default_selected(Path::new("icon.webp"), Kind::Binary));
    }

    #[test]
    fn test_is_default_selected_with_rust_source_returns_true() {
        assert!(is_default_selected(Path::new("src/lib.rs"), Kind::Text));
        assert!(is_default_selected(Path::new("Basic.lean"), Kind::Text));
    }
}
