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

/// Parse [`Kind::as_str`] back into a kind (child extractor CLI).
#[must_use]
pub fn kind_from_label(label: &str) -> Option<Kind> {
    match label {
        "text" => Some(Kind::Text),
        "html" => Some(Kind::Html),
        "xml" => Some(Kind::Xml),
        "json" => Some(Kind::Json),
        "csv" => Some(Kind::Csv),
        "tsv" => Some(Kind::Tsv),
        "pdf" => Some(Kind::Pdf),
        "docx" => Some(Kind::Docx),
        "pptx" => Some(Kind::Pptx),
        "sheet" => Some(Kind::Spreadsheet),
        "odt" => Some(Kind::Odt),
        "odp" => Some(Kind::Odp),
        "epub" => Some(Kind::Epub),
        "rtf" => Some(Kind::Rtf),
        "ipynb" => Some(Kind::Notebook),
        "npy" => Some(Kind::Npy),
        "npz" => Some(Kind::Npz),
        "zip" => Some(Kind::Zip),
        "tar" => Some(Kind::Tar),
        "targz" => Some(Kind::TarGz),
        "binary" => Some(Kind::Binary),
        "unknown" => Some(Kind::Unknown),
        _ => None,
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

/// Short language / format tag for the mill tree (never a mystery for `.lean`).
#[must_use]
pub fn language_label(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if let Some(label) = filename_label(&name) {
        return label;
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(label) = ext_label(&ext.to_ascii_lowercase()) {
            return label;
        }
    }
    classify(path, None).as_str()
}

fn filename_label(name: &str) -> Option<&'static str> {
    Some(match name {
        "dockerfile" | "containerfile" => "docker",
        "makefile" | "gnumakefile" => "make",
        "justfile" => "just",
        "cmakelists.txt" => "cmake",
        "lakefile.lean" => "lean",
        "lakefile.toml" => "toml",
        "gemfile" | "rakefile" => "ruby",
        "procfile" => "procfile",
        "vagrantfile" => "ruby",
        "cargo.lock" => "toml",
        "go.mod" | "go.sum" => "go",
        "build.gradle" | "settings.gradle" => "groovy",
        "podfile" => "ruby",
        "brewfile" => "ruby",
        "earthfile" => "earthfile",
        _ => return None,
    })
}

fn ext_label(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "lean" => "lean",
        "olean" => "olean",
        "agda" | "lagda" => "agda",
        "idr" | "lidr" => "idris",
        "rs" => "rust",
        "py" | "pyi" | "pyw" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" => "c",
        "h" | "hh" | "hpp" | "hxx" => "c",
        "cpp" | "cc" | "cxx" => "cpp",
        "m" => "objc",
        "mm" => "objcpp",
        "cs" => "csharp",
        "fs" | "fsx" | "fsi" => "fsharp",
        "swift" => "swift",
        "rb" | "rake" | "gemspec" | "podspec" => "ruby",
        "php" => "php",
        "lua" => "lua",
        "r" => "r",
        "jl" => "julia",
        "sql" => "sql",
        "dart" => "dart",
        "scala" | "sc" => "scala",
        "groovy" => "groovy",
        "ex" | "exs" => "elixir",
        "heex" => "heex",
        "erl" | "hrl" => "erlang",
        "hs" | "lhs" => "haskell",
        "ml" | "mli" => "ocaml",
        "nim" => "nim",
        "zig" => "zig",
        "v" => "v",
        "d" => "d",
        "pas" => "pascal",
        "pl" | "pm" => "perl",
        "raku" => "raku",
        "proto" => "protobuf",
        "thrift" => "thrift",
        "graphql" | "gql" => "graphql",
        "prisma" => "prisma",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "sol" => "solidity",
        "move" => "move",
        "wgsl" => "wgsl",
        "glsl" => "glsl",
        "hlsl" => "hlsl",
        "metal" => "metal",
        "asm" | "s" => "asm",
        "wat" => "wat",
        "wit" => "wit",
        "clj" | "cljs" => "clojure",
        "edn" => "edn",
        "lisp" | "el" => "lisp",
        "vim" => "vim",
        "nix" => "nix",
        "tf" | "hcl" => "hcl",
        "cue" => "cue",
        "jsonnet" => "jsonnet",
        "bzl" | "bazel" => "starlark",
        "cabal" => "cabal",
        "cmake" => "cmake",
        "make" | "mk" => "make",
        "ninja" => "ninja",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" | "geojson" => "json",
        "md" | "markdown" | "mdx" => "markdown",
        "rst" => "rst",
        "org" => "org",
        "tex" | "bib" => "latex",
        "css" => "css",
        "scss" | "sass" => "scss",
        "less" => "less",
        "html" | "htm" | "xhtml" => "html",
        "xml" | "xsl" | "xslt" | "plist" | "dtd" | "xsd" | "wsdl" => "xml",
        "svg" | "svgz" => "svg",
        "csv" => "csv",
        "tsv" => "tsv",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "ps1" => "powershell",
        "bat" | "cmd" => "batch",
        "diff" | "patch" => "diff",
        "ipynb" => "ipynb",
        "pdf" => "pdf",
        "docx" | "dotx" => "docx",
        "pptx" | "potx" => "pptx",
        "xlsx" | "xlsm" | "xls" | "xltx" | "ods" => "sheet",
        "odt" => "odt",
        "odp" => "odp",
        "epub" => "epub",
        "rtf" => "rtf",
        "npy" => "npy",
        "npz" => "npz",
        "zip" | "zipx" => "zip",
        "tar" => "tar",
        "png" | "jpg" | "jpeg" | "jpe" | "gif" | "webp" | "ico" | "bmp" | "tif" | "tiff"
        | "heic" | "heif" | "avif" | "psd" | "ai" | "eps" | "icns" => "image",
        "woff" | "woff2" | "ttf" | "otf" | "eot" => "font",
        "mp3" | "m4a" | "aac" | "ogg" | "flac" | "wav" => "audio",
        "mp4" | "avi" | "mov" | "mkv" | "webm" => "video",
        "lock" => "lock",
        "sum" => "sum",
        "mod" => "go",
        "work" => "go",
        "csproj" | "fsproj" | "vbproj" | "sln" => "dotnet",
        "gitignore" | "gitattributes" | "dockerignore" | "editorconfig" => "config",
        "env" | "properties" | "ini" | "cfg" | "conf" | "cnf" => "config",
        "tomlrc" | "npmrc" | "nvmrc" | "prettierrc" | "eslintrc" | "babelrc" => "config",
        "zshrc" | "bashrc" | "profile" | "gitconfig" => "config",
        "purs" => "purescript",
        "elm" => "elm",
        "rkt" => "racket",
        "scm" | "ss" => "scheme",
        "f90" | "f95" | "f03" | "for" => "fortran",
        "cob" | "cbl" => "cobol",
        "adb" | "ads" => "ada",
        "cr" => "crystal",
        "nimble" => "nim",
        "re" | "rei" => "reason",
        "res" | "resi" => "rescript",
        "mligo" | "jsligo" => "ligo",
        "wast" => "wat",
        "sml" => "sml",
        "fun" | "sig" => "sml",
        "cl" => "opencl",
        "cu" | "cuh" => "cuda",
        "comp" | "frag" | "vert" => "glsl",
        "puml" | "plantuml" => "uml",
        "dot" | "gv" => "graphviz",
        "ron" => "ron",
        "kdl" => "kdl",
        "hurl" => "hurl",
        "http" | "rest" => "http",
        "jinja" | "j2" | "njk" | "ejs" | "erb" | "twig" | "liquid" | "hbs" | "mustache" => {
            "template"
        }
        "pug" | "jade" | "haml" | "slim" => "template",
        "cshtml" | "razor" => "razor",
        "jsp" | "jspx" => "jsp",
        _ => return None,
    })
}

/// Whether the mill should tick this file after a scan.
///
/// Lockfiles, raster/vector images, virtualenv trees, and other binary
/// media stay in the tree but start unchecked.
#[must_use]
pub fn is_default_selected(path: &Path, kind: Kind) -> bool {
    if kind.is_binary_media() {
        return false;
    }
    !is_lock_file(path) && !is_image_file(path) && !is_venv_path(path)
}

fn is_venv_path(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str().to_str().is_some_and(|name| {
            name.eq_ignore_ascii_case(".venv") || name.eq_ignore_ascii_case("venv")
        })
    })
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
    fn test_is_default_selected_with_venv_path_returns_false() {
        assert!(!is_default_selected(
            Path::new(".venv/lib/python3.11/site.py"),
            Kind::Text
        ));
        assert!(!is_default_selected(
            Path::new("venv/lib/site.py"),
            Kind::Text
        ));
        assert!(!is_default_selected(
            Path::new("src/.venv/foo.py"),
            Kind::Text
        ));
        assert!(is_default_selected(Path::new("src/app.py"), Kind::Text));
    }

    #[test]
    fn test_is_default_selected_with_rust_source_returns_true() {
        assert!(is_default_selected(Path::new("src/lib.rs"), Kind::Text));
        assert!(is_default_selected(Path::new("Basic.lean"), Kind::Text));
    }

    #[test]
    fn test_language_label_with_lean_returns_lean_not_unknown() {
        assert_eq!(language_label(Path::new("Dual/Basic.lean")), "lean");
        assert_eq!(language_label(Path::new("lakefile.lean")), "lean");
        assert_ne!(language_label(Path::new("Basic.lean")), "unknown");
        assert_ne!(language_label(Path::new("Basic.lean")), "text");
    }

    #[test]
    fn test_language_label_with_common_languages_returns_names() {
        assert_eq!(language_label(Path::new("src/lib.rs")), "rust");
        assert_eq!(language_label(Path::new("app.ts")), "typescript");
        assert_eq!(language_label(Path::new("main.py")), "python");
        assert_eq!(language_label(Path::new("Main.agda")), "agda");
        assert_eq!(language_label(Path::new("Dockerfile")), "docker");
    }
}
