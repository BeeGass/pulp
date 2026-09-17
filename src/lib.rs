//! Pulp grinds a local folder of mixed documents into one LLM-ready text file.
//!
//! Named for paper pulp, the verb *to pulp* (extract the juice), and pulp
//! fiction: disposable reading you hand a model. Everything runs on the
//! machine you point it at. Nothing is uploaded.

pub mod classify;
pub mod config;
pub mod error;
pub mod extract;
pub mod manifest;
pub mod pack;
mod pick;
pub mod render;
pub mod tokens;
pub mod tree;
pub mod ui;
pub mod walk;

pub use classify::{Kind, classify, is_default_selected, language_label, looks_binary};
pub use config::{Options, OutputFormat, Selection, TreeMode};
pub use error::Error;
pub use manifest::{ManifestEntry, ScanManifest, scan_manifest};
pub use pack::{FileStatus, Packed, PackedFile, Stats, pack};
