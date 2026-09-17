//! Path include/exclude helpers shared by walk and pack.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::Error;

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
