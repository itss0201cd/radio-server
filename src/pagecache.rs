// pagecache.rs — return a finished track's pages to the kernel.

use std::path::Path;

/// Tell the kernel that the file's cached pages are no longer needed. Tracks
/// are read start-to-finish and not touched again for a long time, so keeping
/// them resident only evicts pages that are still useful.
pub fn forget(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        // SAFETY: the descriptor belongs to a live `File`, and `posix_fadvise`
        // only reads it; it cannot access memory through it. Best-effort: the
        // advice is a hint and any error is harmless.
        unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
}
