//! Pulp grinds a local folder of mixed documents into one LLM-ready text file.
//!
//! Named for paper pulp, the verb *to pulp* (extract the juice), and pulp
//! fiction: disposable reading you hand a model. Everything runs on the
//! machine you point it at. Nothing is uploaded.

pub mod classify;
pub mod config;
pub mod error;
pub mod extract;
pub mod pack;
pub mod render;
pub mod tokens;
pub mod tree;
pub mod ui;
pub mod walk;

pub use classify::{Kind, classify, looks_binary};
pub use config::{Options, OutputFormat, TreeMode};
pub use error::Error;
pub use pack::{FileStatus, Packed, PackedFile, Stats, pack};
