//! Engine shared by every dffy frontend: git access, diffing, highlighting
//! and the view model. It never assumes which process hosts it.
#![warn(missing_docs)]

pub mod cache;
pub mod diff;
pub mod git;
pub mod highlight;
pub mod structural;
pub mod suggest;
pub mod watch;

/// Byte-string types used for paths and contents throughout the API.
pub use gix::bstr;
