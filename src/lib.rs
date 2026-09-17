//! Pulp grinds a local folder of mixed documents into one LLM-ready text file.
//!
//! Named for paper pulp, the verb *to pulp* (extract the juice), and pulp
//! fiction: disposable reading you hand a model. Everything runs on the
//! machine you point it at. Nothing is uploaded.

pub mod classify;
pub mod config;
pub mod error;
mod filter;
pub mod extract;
pub mod manifest;
pub mod pack;
#[cfg(feature = "native")]
mod pick;
pub mod render;
#[cfg(feature = "native")]
mod store;
pub mod tokens;
pub mod tree;
#[cfg(feature = "native")]
pub mod ui;
#[cfg(feature = "native")]
pub mod walk;

pub use classify::{
    Kind, classify, is_default_selected, kind_from_label, language_label, looks_binary,
};
pub use config::{Options, OutputFormat, Selection, TreeMode};
pub use error::Error;
pub use manifest::{ManifestEntry, ScanManifest};
#[cfg(feature = "native")]
pub use manifest::scan_manifest;
pub use pack::{FileStatus, Packed, PackedFile, Stats, pack_entries};
#[cfg(feature = "native")]
pub use pack::{pack, pack_manifest, pack_with_cancel};
