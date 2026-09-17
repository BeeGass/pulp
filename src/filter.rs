//! Path include/exclude helpers shared by walk, pack, and the browser mill.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::Options;
use crate::error::Error;

/// Compiled include/exclude/hidden policy. Same matcher for native and WASM.
pub struct PathPolicy {
    include: Option<GlobSet>,
    exclude: GlobSet,
    hidden: bool,
    follow_archives: bool,
}

impl PathPolicy {
    pub fn from_options(opts: &Options) -> Result<Self, Error> {
        let include = if opts.include.is_empty() {
            None
        } else {
            Some(build_globset(&opts.include)?)
        };
        Ok(Self {
            include,
            exclude: build_globset(&opts.exclude)?,
            hidden: opts.hidden,
            follow_archives: opts.follow_archives,
        })
    }

    /// Whether a path may be emitted as an extracted file.
    #[must_use]
    pub fn keep_emit(&self, relative: &str) -> bool {
        if !self.hidden && is_hidden_rel(relative) {
            return false;
        }
        keep_relative(relative, self.include.as_ref(), &self.exclude)
    }

    /// Whether a path may enter the pipeline (emit or traverse an archive).
    #[must_use]
    pub fn keep_walk(&self, relative: &str) -> bool {
        if !self.hidden && is_hidden_rel(relative) {
            return false;
        }
        if glob_matches(&self.exclude, relative) {
            return false;
        }
        match &self.include {
            None => true,
            Some(set) => {
                glob_matches(set, relative)
                    || (self.follow_archives && looks_like_archive(relative))
            }
        }
    }
}

fn looks_like_archive(relative: &str) -> bool {
    let n = relative.to_ascii_lowercase();
    n.ends_with(".zip") || n.ends_with(".tar") || n.ends_with(".tgz") || n.ends_with(".tar.gz")
}

pub(crate) fn build_globset(patterns: &[String]) -> Result<GlobSet, Error> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if pat.is_empty() {
            continue;
        }
        add_glob(&mut builder, pat)?;
    }
    builder.build().map_err(|err| Error::msg(err.to_string()))
}

pub(crate) fn keep_relative(relative: &str, include: Option<&GlobSet>, exclude: &GlobSet) -> bool {
    if glob_matches(exclude, relative) {
        return false;
    }
    match include {
        None => true,
        Some(set) => glob_matches(set, relative),
    }
}

pub(crate) fn is_hidden_rel(relative: &str) -> bool {
    relative
        .split('/')
        .any(|part| part.starts_with('.') && part != "." && part != "..")
}

pub(crate) fn glob_matches(set: &GlobSet, relative: &str) -> bool {
    if set.is_match(relative) {
        return true;
    }
    Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| set.is_match(name))
}

fn add_glob(builder: &mut GlobSetBuilder, pat: &str) -> Result<(), Error> {
    let glob = Glob::new(pat).map_err(|err| Error::msg(format!("invalid glob {pat}: {err}")))?;
    builder.add(glob);
    let trimmed = pat.trim_start_matches('/');
    if !pat.starts_with("**/") && !trimmed.is_empty() {
        let nested = format!("**/{trimmed}");
        if nested != pat {
            if let Ok(glob) = Glob::new(&nested) {
                builder.add(glob);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Options, default_exclude_globs};

    fn policy(exclude: &[&str], include: &[&str]) -> PathPolicy {
        let opts = Options {
            exclude: exclude.iter().map(|s| (*s).to_string()).collect(),
            include: include.iter().map(|s| (*s).to_string()).collect(),
            hidden: false,
            follow_archives: true,
            ..Options::default()
        };
        PathPolicy::from_options(&opts).unwrap()
    }

    #[test]
    fn test_keep_emit_with_key_glob_excludes_private_key() {
        let p = policy(&["*.key"], &[]);
        assert!(!p.keep_emit("private.key"));
        assert!(p.keep_emit("readme.md"));
    }

    #[test]
    fn test_keep_emit_with_min_js_excludes_bundle() {
        let p = policy(&["*.min.js"], &[]);
        assert!(!p.keep_emit("bundle.min.js"));
        assert!(p.keep_emit("bundle.js"));
    }

    #[test]
    fn test_keep_emit_with_build_dir_does_not_drop_build_rs() {
        let p = policy(&["build/**"], &[]);
        assert!(!p.keep_emit("build/out.js"));
        assert!(p.keep_emit("src/build.rs"));
    }

    #[test]
    fn test_keep_emit_with_target_dir_does_not_drop_targeting_md() {
        let p = policy(&["target/**"], &[]);
        assert!(!p.keep_emit("target/debug/foo"));
        assert!(p.keep_emit("targeting.md"));
    }

    #[test]
    fn test_keep_emit_with_default_excludes_drops_key_not_rust() {
        let opts = Options {
            exclude: default_exclude_globs(),
            ..Options::default()
        };
        let p = PathPolicy::from_options(&opts).unwrap();
        assert!(!p.keep_emit("secrets/id.key"));
        assert!(p.keep_emit("src/lib.rs"));
    }
}
