//! `cargo xtask site`: copy the shared mill UI from `web/` into the static site.
//!
//! `web/` is the source of truth: the local mill embeds it, and the site deploys
//! `site/` as-is, so every shared file is copied rather than referenced.

use std::path::Path;

use anyhow::Context;

/// `(source, destination)` pairs, relative to the workspace root.
pub const COPIES: &[(&str, &str)] = &[
    ("web/pulp.css", "site/pulp.css"),
    ("web/mill.css", "site/mill/mill.css"),
    ("web/mill.js", "site/mill/mill.js"),
    ("web/sample.json", "site/mill/sample.json"),
    ("web/sample-demo.json", "site/mill/demo.json"),
    ("web/fonts/plex-sans.woff2", "site/fonts/plex-sans.woff2"),
    (
        "web/fonts/plex-mono-400.woff2",
        "site/fonts/plex-mono-400.woff2",
    ),
    (
        "web/fonts/plex-mono-500.woff2",
        "site/fonts/plex-mono-500.woff2",
    ),
    ("web/fonts/LICENSE.md", "site/fonts/LICENSE.md"),
    ("web/fonts/OFL.txt", "site/fonts/OFL.txt"),
];

/// Copy every stale pair and return the destinations that changed.
pub fn sync(root: &Path) -> anyhow::Result<Vec<&'static str>> {
    let mut changed = Vec::new();
    for &(from, to) in COPIES {
        let src = root.join(from);
        let dst = root.join(to);
        let bytes = std::fs::read(&src).with_context(|| format!("read {}", src.display()))?;
        if std::fs::read(&dst).ok().as_deref() == Some(bytes.as_slice()) {
            continue;
        }
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&dst, &bytes).with_context(|| format!("write {}", dst.display()))?;
        changed.push(to);
    }
    Ok(changed)
}

/// Destinations whose bytes differ from their source (or are missing).
pub fn stale(root: &Path) -> Vec<&'static str> {
    COPIES
        .iter()
        .filter(|(from, to)| {
            let src = std::fs::read(root.join(from)).ok();
            src.is_none() || src != std::fs::read(root.join(to)).ok()
        })
        .map(|&(_, to)| to)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_site_copies_match_web_sources() {
        let stale = stale(&crate::cargo::workspace_root());
        assert!(
            stale.is_empty(),
            "site/ is out of date for {stale:?}; run `cargo xtask site`"
        );
    }

    #[test]
    fn test_sync_with_scratch_tree_copies_then_settles() {
        let dir = std::env::temp_dir().join(format!("pulp-xtask-site-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for &(from, _) in COPIES {
            let src = dir.join(from);
            std::fs::create_dir_all(src.parent().unwrap()).unwrap();
            std::fs::write(&src, from.as_bytes()).unwrap();
        }
        let first = sync(&dir).unwrap().len();
        let settled = stale(&dir).is_empty() && sync(&dir).unwrap().is_empty();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(first, COPIES.len());
        assert!(settled);
    }
}
