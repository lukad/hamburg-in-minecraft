//! `hhmc fetch`: downloads every file listed in `data/sources.toml`, verifies its
//! sha256 and never downloads a verified file again.

mod attribution;
mod download;
mod expand;
pub mod remote_zip;
mod sources;

pub use attribution::render_attribution;
pub use download::{FetchOptions, fetch};
pub use expand::expand;
pub use sources::{Source, SourceFile, Sources};
