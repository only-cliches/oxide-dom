use std::env;
use std::path::{Path, PathBuf};

use crate::bundle::{self, BundleError, Generated};

/// Bundle a solite source directory from `build.rs` into `$OUT_DIR/out_name`.
///
/// The generated file exposes `ENTRY` and `modules()`, suitable for:
///
/// ```ignore
/// mod ui_bundle {
///     include!(concat!(env!("OUT_DIR"), "/ui_bundle.rs"));
/// }
/// ```
///
/// Emits `cargo:rerun-if-changed` for the source directory and for every source
/// file visited by the module graph.
pub fn bundle_for_cargo(
    src_dir: impl AsRef<Path>,
    out_name: impl AsRef<Path>,
) -> Result<Generated, BundleError> {
    let out_dir = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| BundleError::MissingOutDir)?;
    let out = out_dir.join(out_name.as_ref());
    let generated = bundle::bundle_to_file(src_dir.as_ref(), &out)?;

    println!("cargo:rerun-if-changed={}", src_dir.as_ref().display());
    for source in &generated.sources {
        println!("cargo:rerun-if-changed={}", source.display());
    }

    Ok(generated)
}
