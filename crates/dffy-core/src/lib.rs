//! What a diff means, for any program that shows one: git access, the line
//! and intra-line diff, the syntax-aware diff, highlighting, unified-diff
//! parsing, the diff document model (files, rows, line states, positions,
//! navigation, search), annotations placed on its files' lines, and where
//! names are defined. The
//! `dffy` binary is one host of it.
//!
//! Core decides, the host draws. The host supplies the executor, text
//! measurement (widths, wrapping), colors and drawing. Core has no thread
//! pool or event loop: the host picks the thread each call runs on, and
//! [`document::Document::missing`] names the work the rows it's about to
//! show still lack. Some calls fork scoped threads for one job and join
//! them before returning; the watcher and the cache's trimming run threads
//! of their own.
//!
//! # Layers
//!
//! A module imports only from its own layer and the layers below it:
//!
//! | Layer | Modules | Built on |
//! |---|---|---|
//! | 0 | [`diff`], [`patch`], [`view`], [`conflict`] | memchr and rustc-hash; no git, no parser, no I/O or threads |
//! | 1 | [`highlight`], [`structural`] | tree-sitter |
//! | 2 | [`git`], [`watch`], [`cache`] | gix and the filesystem |
//! | 3 | [`document`], [`annotation`], [`navigate`] | everything below |
//!
//! Layer 1 is the exception: it stores results through [`cache`], names
//! blobs by [`git::ObjectId`], and hashes grammar identities with gix.
//! [`suggest`] imports nothing; [`git`] uses it.
//!
//! The API is 0.x and unstable: anything may change in any release.
#![warn(missing_docs)]

pub mod annotation;
pub mod cache;
pub mod conflict;
pub mod diff;
pub mod document;
pub mod git;
pub mod highlight;
pub mod navigate;
pub mod patch;
pub mod structural;
pub mod suggest;
pub mod view;
pub mod watch;

/// Byte-string types used for paths and contents throughout the API.
pub use gix::bstr;
