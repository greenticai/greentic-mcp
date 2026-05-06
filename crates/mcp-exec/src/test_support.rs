//! Test-only helpers for filesystem fixtures.

use std::path::PathBuf;
use std::sync::Once;

use tempfile::TempDir;

/// Create a `TempDir` rooted at `<crate>/.greentic-mcp-tests/` instead of the
/// system temp directory.
///
/// On macOS, `std::env::temp_dir()` resolves through `/var → /private/var`,
/// which causes `Path::starts_with` and `assert_eq!` checks against
/// `tempfile::tempdir()` results to fail when one side has been canonicalized.
/// Anchoring under the workspace avoids that symlink chain entirely.
pub fn local_tempdir() -> TempDir {
    static INIT: Once = Once::new();
    let root = base_dir();
    INIT.call_once(|| {
        std::fs::create_dir_all(&root).expect("create local tempdir root");
    });
    tempfile::Builder::new()
        .prefix("t-")
        .tempdir_in(&root)
        .expect("local tempdir")
}

fn base_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".greentic-mcp-tests")
}
