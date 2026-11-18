//! Measures the agent host for its state snapshots: memory, CPU load and the
//! disk below `--root`. Anything a platform cannot measure stays `None`, and
//! the controller then falls back to the configured limits.
use std::path::Path;

use super::protocol::Resources;

const MIB: u64 = 1024 * 1024;

/// Measure the host now. Reads `/proc` and calls `statvfs`, so run it off the
/// async workers: a hung network file system would block the call.
pub fn measure(root: &Path) -> Resources {
    let (memory_total_mb, memory_available_mb) = memory();
    let (disk_total_mb, disk_free_mb) = disk(root).unzip();
    Resources {
        memory_total_mb,
        memory_available_mb,
        cpus: std::thread::available_parallelism()
            .ok()
            .and_then(|count| u32::try_from(count.get()).ok()),
        load: load(),
        disk_total_mb,
        disk_free_mb,
    }
}

#[cfg(target_os = "linux")]
fn memory() -> (Option<u64>, Option<u64>) {
    std::fs::read_to_string("/proc/meminfo")
        .map(|text| parse_meminfo(&text))
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn memory() -> (Option<u64>, Option<u64>) {
    let mut bytes: u64 = 0;
    let mut size = size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer; `bytes` and `size` describe it.
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    // macOS has no `MemAvailable` equivalent that `libc` exposes; agents
    // there report the total only and placement uses the configured limit.
    ((result == 0).then_some(bytes / MIB), None)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn memory() -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, in MiB.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
            Some(kib / 1024)
        })
    };
    (field("MemTotal"), field("MemAvailable"))
}

#[cfg(unix)]
fn load() -> Option<f64> {
    let mut average = [0f64; 1];
    // SAFETY: the buffer holds the one sample requested.
    let samples = unsafe { libc::getloadavg(average.as_mut_ptr(), 1) };
    (samples == 1 && average[0].is_finite()).then_some(average[0])
}

#[cfg(not(unix))]
fn load() -> Option<f64> {
    None
}

/// Unprivileged-free space of the file system holding `root`, in MiB.
pub(crate) fn free_disk_mb(root: &Path) -> Option<u64> {
    disk(root).map(|(_, free)| free)
}

/// Total and unprivileged-free space of the file system holding `root`, in MiB.
#[cfg(unix)]
fn disk(root: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(root.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `stat` is a writable statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and filled the structure.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::useless_conversion)] // The field widths differ by platform.
    let (blocks, available, fragment) = (
        u64::from(stat.f_blocks),
        u64::from(stat.f_bavail),
        u64::from(stat.f_frsize),
    );
    Some((
        blocks.saturating_mul(fragment) / MIB,
        available.saturating_mul(fragment) / MIB,
    ))
}

#[cfg(not(unix))]
fn disk(_root: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_reports_total_and_available() {
        let text = "MemTotal:       16314768 kB\nMemFree:          812344 kB\nMemAvailable:    9437184 kB\n";
        assert_eq!(parse_meminfo(text), (Some(15932), Some(9216)));
        assert_eq!(parse_meminfo("MemTotal: 2048 kB\n"), (Some(2), None));
        assert_eq!(parse_meminfo("garbage"), (None, None));
    }

    #[test]
    fn measuring_this_host_finds_its_disk() {
        let resources = measure(Path::new("."));
        assert!(resources.cpus.is_some_and(|cpus| cpus > 0));
        #[cfg(unix)]
        {
            let (total, free) = (
                resources.disk_total_mb.unwrap(),
                resources.disk_free_mb.unwrap(),
            );
            assert!(total > 0 && free <= total, "{total} {free}");
            assert!(resources.load.is_some_and(|load| load >= 0.0));
        }
        #[cfg(target_os = "linux")]
        assert!(resources.memory_available_mb <= resources.memory_total_mb);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(resources.memory_total_mb.is_some_and(|total| total > 0));
    }
}
