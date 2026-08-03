//! Shared deterministic temporary-file fixtures for backend tests.

use std::fs::File;
use std::path::PathBuf;
use std::{env, fs, process};

/// Runs `run` against an open read-only file containing exactly `contents`.
///
/// The file lives under the system temporary directory with a name derived
/// from `test` and the process id, so parallel tests in one process must
/// pass distinct `test` names. The file is removed after `run` returns.
pub(crate) fn with_file_content<T>(
    test: &str,
    contents: &[u8],
    run: impl FnOnce(&mut File) -> T,
) -> T {
    let path: PathBuf = env::temp_dir().join(format!("range-replay-{test}-{}", process::id()));
    fs::write(&path, contents).expect("fixture file is writable");
    let mut file = File::open(&path).expect("fixture file opens");

    let result = run(&mut file);

    drop(file);
    fs::remove_file(&path).expect("fixture file is removable");

    result
}
