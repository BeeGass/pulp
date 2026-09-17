//! Unicode directory map for the top of a pulp dump.

use std::collections::BTreeMap;

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
    if root_label.ends_with('/') {
        out.push_str(root_label);
    } else {
        out.push_str(root_label);
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

fn insert_path(root: &mut Dir, path: &str) {
    let normalized = path.replace('\\', "/");
    let last_is_dir = normalized.ends_with('/');
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect();
    if parts.is_empty() {
        return;
    }
    insert(root, &parts, last_is_dir);
}

fn insert(dir: &mut Dir, parts: &[&str], last_is_dir: bool) {
    let Some((name, rest)) = parts.split_first() else {
        return;
    };
    if rest.is_empty() && !last_is_dir {
        dir.children
            .entry((*name).to_string())
            .or_insert(Node::File);
        return;
    }

    let node = dir
        .children
        .entry((*name).to_string())
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
        out.push_str(name);
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
