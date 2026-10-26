//! The memory this host leaves the joint walks of the specs it is sent.
//!
//! A joint walk holds every state it has seen, so a model that never ends
//! fills the host until the host kills the process, with every tenant on it.
//! The walks of one process share [`HOST_ROOM`]: together they hold at most
//! half of the memory the host leaves free to them. Free is what every
//! process sharing a limit with this one leaves, not just this server: one
//! sandbox runs many app servers under one cgroup, and the kernel kills
//! whichever crosses it. Half, because a walk's count of what it holds is
//! an estimate (the allocator rounds and fragments), and the server keeps
//! the rest to go on serving while a walk runs. Free memory is read again
//! each time a walk asks for more, so the room follows the host as it
//! changes.

use std::sync::atomic::{AtomicUsize, Ordering};

use temper_verify::composite::Room;

const MB: usize = 1 << 20;

/// The memory this process's joint walks hold.
pub struct HostRoom {
    held: AtomicUsize,
}

/// The one room the joint walks of this process share.
pub static HOST_ROOM: HostRoom = HostRoom {
    held: AtomicUsize::new(0),
};

impl Room for HostRoom {
    fn take(&self, bytes: usize) -> Result<(), String> {
        let mut held = self.held.load(Ordering::Relaxed);
        loop {
            let free = free_memory()
                .ok_or_else(|| "this host's free memory could not be read".to_string())?;
            // The walks' own memory is counted as used in `free`.
            let room = free.saturating_add(held) / 2;
            let after = held.saturating_add(bytes);
            if after > room {
                return Err(format!(
                    "this host leaves joint walks {} MB (half of the {} MB free to them) \
                     and they hold {} MB",
                    room / MB,
                    free.saturating_add(held) / MB,
                    held / MB
                ));
            }
            match self
                .held
                .compare_exchange(held, after, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(now) => held = now,
            }
        }
    }

    fn give(&self, bytes: usize) {
        self.held.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// The memory this process can still take: the least of what the machine
/// leaves available and what each cgroup above it with a limit leaves,
/// counting reclaimable file cache as free, as the kernel does.
#[cfg(target_os = "linux")]
fn free_memory() -> Option<usize> {
    use std::path::Path;

    linux_free_memory(&|path: &Path| std::fs::read_to_string(path).ok()) // determinism-ok: host memory read
}

#[cfg(target_os = "linux")]
fn linux_free_memory(read: &impl Fn(&std::path::Path) -> Option<String>) -> Option<usize> {
    use std::path::Path;

    let mut free = field(&read(Path::new("/proc/meminfo"))?, "MemAvailable:")?.saturating_mul(1024);
    let groups = read(Path::new("/proc/self/cgroup"))?;
    // Each memory hierarchy names this process's group. Every ancestor can
    // impose a tighter limit, shared with the other processes below it.
    for line in groups.lines() {
        let mut parts = line.splitn(3, ':');
        let hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        let (root, files) = if hierarchy == "0" && controllers.is_empty() {
            (
                Path::new("/sys/fs/cgroup"),
                ["memory.max", "memory.current", "inactive_file"],
            )
        } else if controllers.split(',').any(|c| c == "memory") {
            (
                Path::new("/sys/fs/cgroup/memory"),
                [
                    "memory.limit_in_bytes",
                    "memory.usage_in_bytes",
                    "total_inactive_file",
                ],
            )
        } else {
            continue;
        };
        let own = root.join(path.trim_start_matches('/'));
        for dir in own.ancestors().take_while(|dir| dir.starts_with(root)) {
            if let Some(left) = group_free(read, dir, files) {
                free = free.min(left);
            }
        }
    }
    Some(free)
}

/// What the cgroup at `dir` leaves under its limit, from its limit, usage
/// and inactive file cache files.
#[cfg(target_os = "linux")]
fn group_free(
    read: &impl Fn(&std::path::Path) -> Option<String>,
    dir: &std::path::Path,
    [limit, usage, inactive]: [&str; 3],
) -> Option<usize> {
    let limit: usize = read(&dir.join(limit))?.trim().parse().ok()?;
    let usage: usize = read(&dir.join(usage))?.trim().parse().ok()?;
    let inactive = field(&read(&dir.join("memory.stat"))?, inactive)?;
    Some(limit.saturating_sub(usage.saturating_sub(inactive)))
}

/// The number after `name` on its line of a procfs or cgroup stat file.
#[cfg(target_os = "linux")]
fn field(text: &str, name: &str) -> Option<usize> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.split_whitespace().next())?
        .parse()
        .ok()
}

/// Free and inactive pages across the host, after every process's usage.
/// Speculative pages are already included in `free_count`; active, wired
/// and compressed pages are not available for a walk.
#[cfg(target_os = "macos")]
fn free_memory() -> Option<usize> {
    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    let mut stats = std::mem::MaybeUninit::<libc::vm_statistics64>::uninit();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the output buffer and count match HOST_VM_INFO64. The host
    // port returned by mach_host_self is released after the call.
    #[allow(deprecated)] // libc's Mach API is deprecated in favor of mach2.
    let (status, page_size) = unsafe {
        let host = libc::mach_host_self();
        let status = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr().cast(),
            &mut count,
        );
        mach_port_deallocate(libc::mach_task_self(), host);
        (status, libc::sysconf(libc::_SC_PAGESIZE))
    };
    if status != libc::KERN_SUCCESS || count != libc::HOST_VM_INFO64_COUNT {
        return None;
    }
    // SAFETY: a successful host_statistics64 call initialized the buffer.
    let stats = unsafe { stats.assume_init() };
    let pages = (stats.free_count as usize).saturating_add(stats.inactive_count as usize);
    pages.checked_mul(usize::try_from(page_size).ok()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_memory() -> Option<usize> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
