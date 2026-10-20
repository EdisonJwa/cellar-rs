//! A scratch directory for tests that have to write a real file.
//!
//! `std::env::temp_dir()` is the obvious choice and the wrong one: it is not guaranteed to be
//! writable by the test process, and two test binaries running at once collide on a fixed file
//! name. The order of preference here is:
//!
//! 1. `CARGO_TARGET_TMPDIR`, which Cargo sets for integration tests when it can.
//! 2. A directory inside the target tree, which the build has already proved it can write to.
//! 3. The system temporary directory, made unique per process and call.
//!
//! The fallback chain matters on sandboxed builds, where `%TEMP%` can exist and still refuse
//! writes — the target tree is the one place the build controls.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The directory scratch files go in, creating it if necessary.
fn scratch_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_TMPDIR") {
        let dir = PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    // `CARGO_MANIFEST_DIR/target/test-scratch` sits inside the build tree, which is writable by
    // construction: Cargo has been writing artefacts there for the whole build.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch");
    if std::fs::create_dir_all(&dir).is_ok() {
        return dir;
    }
    std::env::temp_dir()
}

/// A writable path for one test file, unique to this process and call.
///
/// The file is not created; callers create it. `name` is used as a prefix so a failed run leaves
/// a file that says which test produced it.
pub fn scratch_file(name: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    scratch_dir().join(format!("{name}-{}-{unique}.tmp", std::process::id()))
}

/// Removes a scratch file, ignoring the case where it was never written.
pub fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}
