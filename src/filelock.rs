//! Safe read-modify-write of the room's own state files: the link status cache and the tag
//! overlays.
//!
//! Every one of those files is a whole-file read-modify-write, and every one has more than one
//! writer: the page server handles each POST on its own task of a multi-threaded runtime, the
//! server ALSO runs the scheduled passes (link-check, tag-suggest) on a second kernel over the same
//! files, and the `cms-*` bins are separate processes that write them too. An unguarded
//! read-modify-write there loses decisions silently: sixteen concurrent tag approvals kept one or
//! two, and a link-check pass that wrote back the copy it loaded at its start undid every keep
//! clicked while it ran.
//!
//! Two rules, both here so no caller can get one without the other:
//!
//! - **Serialize writers per file** with an exclusive advisory lock on a sidecar `<file>.lock`
//!   ([`with_lock`]). It is an OS file lock (`flock` on unix), so it holds between threads, between
//!   the serving and the maintenance kernel, and between processes alike — a `Mutex` would cover
//!   only the first. The lock is on a sidecar rather than the data file because the data file is
//!   REPLACED on every write, and a lock on a replaced inode protects nothing.
//! - **Replace atomically** ([`write_atomic`]): write a temp file in the same directory, then
//!   rename it over the target. A reader (the room renders these files on every request, without
//!   the lock) sees the old file or the new one, never a torn half.
//!
//! Readers never take the lock: with atomic replacement a read is always of a whole file, and the
//! lock is only about two writers interleaving their read and their write.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The sidecar lock file for `path`: `<file>.lock` beside it.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Run `f` while holding the exclusive write lock for `path`. Blocks until the lock is free.
///
/// The read that `f` makes must happen INSIDE the closure: the whole point is that the read and
/// the write are one step no other writer can split.
///
/// # Errors
///
/// When the lock file cannot be created or locked (an unwritable directory). The caller decides
/// whether that is fatal; it is never silently skipped here, since running the update unlocked is
/// exactly the lost-update this module exists to prevent.
pub(crate) fn with_lock<T>(path: &Path, f: impl FnOnce() -> T) -> std::io::Result<T> {
    with_locks(&[path], f)
}

/// [`with_lock`] over several files at once, for an update that must see and change them as one
/// (the overlay migration). The locks are taken in the order given; every other writer holds ONE
/// lock at a time and never nests, so a multi-file holder cannot deadlock against them — keep it
/// that way, or give every multi-lock caller the same order.
pub(crate) fn with_locks<T>(paths: &[&Path], f: impl FnOnce() -> T) -> std::io::Result<T> {
    let mut held = Vec::with_capacity(paths.len());
    for path in paths {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(path))?;
        lock.lock()?;
        held.push(lock);
    }
    let out = f();
    // Dropping a handle releases its lock too; unlocking explicitly says when.
    for lock in held.into_iter().rev() {
        let _ = lock.unlock();
    }
    Ok(out)
}

/// Replace `path` with `bytes` atomically: a uniquely named temp file in the same directory
/// (so the rename never crosses a filesystem), flushed, then renamed over the target.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// The lost update this module exists for, at its smallest: N threads each append one line
    /// to the same file under `with_lock`, and every line survives.
    #[test]
    fn locked_read_modify_writes_lose_nothing() {
        const N: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("counter.txt"));
        let gate = Arc::new(Barrier::new(N));
        let threads: Vec<_> = (0..N)
            .map(|i| {
                let (path, gate) = (Arc::clone(&path), Arc::clone(&gate));
                std::thread::spawn(move || {
                    gate.wait();
                    with_lock(&path, || {
                        let mut text = std::fs::read_to_string(&*path).unwrap_or_default();
                        // Widen the window an unlocked version would lose in.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        text.push_str(&format!("{i}\n"));
                        write_atomic(&path, text.as_bytes()).unwrap();
                    })
                    .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = std::fs::read_to_string(&*path).unwrap();
        assert_eq!(text.lines().count(), N, "{text}");
    }

    /// An atomic write leaves no temp file behind and replaces the content whole.
    #[test]
    fn an_atomic_write_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["f.json".to_string()], "{names:?}");
    }
}
