//! Unicode directory map for the top of a pulp dump.

use std::borrow::Cow;
use std::collections::BTreeMap;

/// A path as it may be printed on one line of a dump.
///
/// File and archive member names may hold newlines, other control
/// characters, and code points that print as nothing or reorder the text
/// around them. Printed raw, they split a `FILE:` header or a Markdown
/// heading, fake a tree line, or make a name read as something else. These
/// are written as Rust-style escapes (`\n`, `\u{202e}`):
///
/// - control characters (Unicode category Cc);
/// - format characters (Cf), such as the bidi marks, overrides, and
///   isolates, the soft hyphen, and zero-width spaces, except the zero-width
///   non-joiner and joiner (U+200C, U+200D), which emoji sequences and some
///   scripts need;
/// - the line and paragraph separators U+2028 and U+2029;
/// - the Hangul fillers U+115F, U+1160, U+3164, and U+FFA0, which print
///   blank;
/// - the noncharacters U+FFFE and U+FFFF.
///
/// Every other character is kept.
#[must_use]
pub fn display_path(path: &str) -> Cow<'_, str> {
    if !path.chars().any(needs_display_escape) {
        return Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len() + 8);
    for c in path.chars() {
        if needs_display_escape(c) {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                other => out.extend(other.escape_unicode()),
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Whether [`display_path`] escapes `c`; its doc lists the characters.
fn needs_display_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            // Format characters (Cf) other than U+200C and U+200D.
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'
                | '\u{200E}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
                // Separators, blank fillers, and noncharacters.
                | '\u{2028}'
                | '\u{2029}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{3164}'
                | '\u{FFA0}'
                | '\u{FFFE}'
                | '\u{FFFF}'
        )
}

/// Split a browser file-grant into a tree label and the paths under it.
///
/// A directory pick prefixes every path with that directory's name. When
/// every path shares that first segment, the segment is the label and the
/// remainder are the diagram paths. Otherwise the label is `.` and the
/// paths stay as given.
#[must_use]
pub fn split_grant_root(paths: &[String]) -> (String, Vec<String>) {
    let parts: Vec<Vec<String>> = paths
        .iter()
        .map(|path| path_parts(path))
        .filter(|part| !part.is_empty())
        .collect();
    let Some(first) = parts.first() else {
        return (".".to_string(), Vec::new());
    };
    let label = first[0].clone();
    let shared = parts.iter().all(|part| part.len() > 1 && part[0] == label);
    if !shared {
        let kept = parts.into_iter().map(|part| part.join("/")).collect();
        return (".".to_string(), kept);
    }
    let stripped = parts.into_iter().map(|part| part[1..].join("/")).collect();
    (label, stripped)
}

fn path_parts(path: &str) -> Vec<String> {
    path.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .map(str::to_string)
        .collect()
}

/// Build a `tree(1)`-style listing of `paths` under `root_label`.
///
/// Intermediate directories are inferred from separators. Entries are sorted
/// lexicographically at each level. `root_label` is printed first with a
/// trailing `/` if it does not already have one. Empty `paths` yields that
/// single line.
#[must_use]
pub fn render_tree(root_label: &str, paths: &[String]) -> String {
    let mut root = Dir::default();
    for path in paths {
        insert_path(&mut root, path);
    }

    let mut out = String::new();
    out.push_str(&display_path(root_label));
    if !root_label.ends_with('/') {
        out.push('/');
    }
    out.push('\n');
    write_children(&root, "", &mut out);
    out
}

#[derive(Default)]
struct Dir {
    children: BTreeMap<String, Node>,
}

enum Node {
    File,
    Dir(Dir),
}

/// Deepest directory level the map draws. A path nested deeper keeps its
/// remaining segments on one line, so an archive member named `a/a/a/…`
/// thousands of levels deep cannot make the map grow with the square of its
/// depth.
const MAX_TREE_DEPTH: usize = 64;

fn insert_path(root: &mut Dir, path: &str) {
    let normalized = path.replace('\\', "/");
    let last_is_dir = normalized.ends_with('/');
    let mut parts: Vec<Cow<'_, str>> = normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .map(Cow::Borrowed)
        .collect();
    if parts.is_empty() {
        return;
    }
    if parts.len() > MAX_TREE_DEPTH {
        let tail = parts[MAX_TREE_DEPTH - 1..].join("/");
        parts.truncate(MAX_TREE_DEPTH - 1);
        parts.push(Cow::Owned(tail));
    }
    insert(root, &parts, last_is_dir);
}

fn insert(dir: &mut Dir, parts: &[Cow<'_, str>], last_is_dir: bool) {
    let Some((name, rest)) = parts.split_first() else {
        return;
    };
    if rest.is_empty() && !last_is_dir {
        dir.children.entry(name.to_string()).or_insert(Node::File);
        return;
    }

    let node = dir
        .children
        .entry(name.to_string())
        .or_insert_with(|| Node::Dir(Dir::default()));
    if matches!(node, Node::File) {
        *node = Node::Dir(Dir::default());
    }
    let Node::Dir(sub) = node else {
        return;
    };
    if !rest.is_empty() {
        insert(sub, rest, last_is_dir);
    }
}

fn write_children(dir: &Dir, prefix: &str, out: &mut String) {
    let len = dir.children.len();
    for (idx, (name, child)) in dir.children.iter().enumerate() {
        let is_last = idx + 1 == len;
        out.push_str(prefix);
        out.push_str(if is_last { "└── " } else { "├── " });
        out.push_str(&display_path(name));
        match child {
            Node::Dir(sub) => {
                if !name.ends_with('/') {
                    out.push('/');
                }
                out.push('\n');
                let mut next = String::with_capacity(prefix.len() + 4);
                next.push_str(prefix);
                next.push_str(if is_last { "    " } else { "│   " });
                write_children(sub, &next, out);
            }
            Node::File => out.push('\n'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_tree_with_nested_paths_returns_sorted_unicode_tree() {
        let rendered = render_tree(
            "pulp",
            &[
                "src/render.rs".to_string(),
                "src/tree.rs".to_string(),
                "README.md".to_string(),
            ],
        );
        let expected = "\
pulp/
├── README.md
└── src/
    ├── render.rs
    └── tree.rs
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn test_render_tree_with_empty_paths_returns_root_slash() {
        assert_eq!(render_tree("src", &[]), "src/\n");
        assert_eq!(render_tree("src/", &[]), "src/\n");
    }

    #[test]
    fn test_split_grant_root_with_shared_directory_strips_label() {
        let (label, paths) = split_grant_root(&[
            "website/README.md".to_string(),
            "website/src/main.rs".to_string(),
        ]);
        assert_eq!(label, "website");
        assert_eq!(paths, ["README.md", "src/main.rs"]);
    }

    #[test]
    fn test_split_grant_root_with_loose_files_keeps_dot_label() {
        let (label, paths) = split_grant_root(&["a.rs".to_string(), "notes\\b.md".to_string()]);
        assert_eq!(label, ".");
        assert_eq!(paths, ["a.rs", "notes/b.md"]);
    }

    #[test]
    fn test_split_grant_root_with_mixed_prefixes_keeps_paths() {
        let (label, paths) =
            split_grant_root(&["website/a.rs".to_string(), "other/b.rs".to_string()]);
        assert_eq!(label, ".");
        assert_eq!(paths, ["website/a.rs", "other/b.rs"]);
    }

    #[test]
    fn test_display_path_with_control_and_bidi_chars_returns_escapes() {
        assert_eq!(display_path("src/lib.rs"), "src/lib.rs");
        assert!(matches!(display_path("src/lib.rs"), Cow::Borrowed(_)));
        assert_eq!(display_path("a\nb\tc\rd"), "a\\nb\\tc\\rd");
        assert_eq!(display_path("x\u{1b}y\u{0}z"), "x\\u{1b}y\\u{0}z");
        assert_eq!(display_path("evil\u{202e}txt.exe"), "evil\\u{202e}txt.exe");
        assert_eq!(display_path("line\u{2028}sep"), "line\\u{2028}sep");
        assert_eq!(display_path("café `x` <&>"), "café `x` <&>");
        // Invisible marks and spaces that would make one name print as another.
        assert_eq!(display_path("h\u{200b}.txt"), "h\\u{200b}.txt");
        assert_eq!(display_path("f\u{61c}g.txt"), "f\\u{61c}g.txt");
        assert_eq!(display_path("m\u{180e}n"), "m\\u{180e}n");
        assert_eq!(display_path("a\u{2060}b\u{2064}c"), "a\\u{2060}b\\u{2064}c");
        // Joiners stay: emoji sequences and some scripts need them.
        assert_eq!(
            display_path("\u{1f469}\u{200d}\u{1f52c}"),
            "\u{1f469}\u{200d}\u{1f52c}"
        );
        assert_eq!(display_path("a\u{200c}b"), "a\u{200c}b");
        // Each of these prints as `h.txt` when left raw.
        for (name, shown) in [
            ("h\u{ad}.txt", "h\\u{ad}.txt"),
            ("h\u{206a}.txt", "h\\u{206a}.txt"),
            ("h\u{3164}.txt", "h\\u{3164}.txt"),
            ("h\u{fff9}.txt", "h\\u{fff9}.txt"),
            ("h\u{e0041}.txt", "h\\u{e0041}.txt"),
        ] {
            assert_eq!(display_path(name), shown);
        }
    }

    #[test]
    fn test_display_path_with_each_format_char_and_filler_returns_escape() {
        // The Unicode format category (Cf), then the fillers that print blank.
        let escaped: [(u32, u32); 25] = [
            (0xAD, 0xAD),
            (0x600, 0x605),
            (0x61C, 0x61C),
            (0x6DD, 0x6DD),
            (0x70F, 0x70F),
            (0x890, 0x891),
            (0x8E2, 0x8E2),
            (0x180E, 0x180E),
            (0x200B, 0x200B),
            (0x200E, 0x200F),
            (0x202A, 0x202E),
            (0x2060, 0x2064),
            (0x2066, 0x206F),
            (0xFEFF, 0xFEFF),
            (0xFFF9, 0xFFFB),
            (0x110BD, 0x110BD),
            (0x110CD, 0x110CD),
            (0x13430, 0x1343F),
            (0x1BCA0, 0x1BCA3),
            (0x1D173, 0x1D17A),
            (0xE0001, 0xE0001),
            (0xE0020, 0xE007F),
            (0x115F, 0x1160),
            (0x3164, 0x3164),
            (0xFFA0, 0xFFA0),
        ];
        for (first, last) in escaped {
            for code in first..=last {
                let c = char::from_u32(code).unwrap();
                let name = format!("a{c}b");
                assert_eq!(
                    display_path(&name),
                    format!("a\\u{{{code:x}}}b"),
                    "U+{code:04X}"
                );
            }
        }
        for kept in ['\u{200c}', '\u{200d}', '\u{2065}', '\u{1161}', 'é', '中'] {
            let name = format!("a{kept}b");
            assert_eq!(display_path(&name), name, "U+{:04X}", u32::from(kept));
        }
    }

    #[test]
    fn test_render_tree_with_newline_in_name_returns_one_line_per_entry() {
        let rendered = render_tree(
            "root\nlabel",
            &["a\nFILE: fake.txt".to_string(), "b.rs".to_string()],
        );
        let expected = "\
root\\nlabel/
├── a\\nFILE: fake.txt
└── b.rs
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn test_render_tree_with_path_deeper_than_cap_returns_tail_on_one_line() {
        let deep = vec!["d"; 5000].join("/") + "/leaf.txt";
        let rendered = render_tree("root", &[deep]);
        assert_eq!(rendered.lines().count(), 1 + MAX_TREE_DEPTH);
        assert!(rendered.len() < 64 * 1024, "{} bytes", rendered.len());
        let last = rendered.lines().last().unwrap();
        assert!(last.ends_with("d/d/leaf.txt"), "{last}");
    }

    #[test]
    fn test_render_tree_with_deep_path_infers_intermediate_dirs() {
        let rendered = render_tree("root", &["a/b/c.rs".to_string()]);
        let expected = "\
root/
└── a/
    └── b/
        └── c.rs
";
        assert_eq!(rendered, expected);
    }
}
