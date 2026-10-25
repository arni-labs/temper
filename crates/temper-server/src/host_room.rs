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

    let read = |path: &Path| std::fs::read_to_string(path).ok(); // determinism-ok: host memory read
    let mut free = field(&read(Path::new("/proc/meminfo"))?, "MemAvailable:")?.saturating_mul(1024);
    // cgroup v2 names the process's group on its "0::" line. Every group from
    // there up to the mount's root may set a limit the process shares with
    // the others in it; a group without one says "max" and is skipped.
    let root = Path::new("/sys/fs/cgroup");
    let own = read(Path::new("/proc/self/cgroup"))
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
        })
        .unwrap_or_default();
    let own = root.join(own.trim().trim_start_matches('/'));
    let v2 = ["memory.max", "memory.current", "inactive_file"];
    for dir in own.ancestors().take_while(|dir| dir.starts_with(root)) {
        if let Some(left) = group_free(&read, dir, v2) {
            free = free.min(left);
        }
    }
    // cgroup v1 keeps memory in its own hierarchy; unlimited reads near 2^63.
    let v1 = [
        "memory.limit_in_bytes",
        "memory.usage_in_bytes",
        "total_inactive_file",
    ];
    if let Some(left) = group_free(&read, &root.join("memory"), v1) {
        free = free.min(left);
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

/// A development host: the machine's memory less this process's own.
#[cfg(target_os = "macos")]
fn free_memory() -> Option<usize> {
    let mut bytes: u64 = 0;
    let mut size = size_of::<u64>();
    // SAFETY: hw.memsize is a u64; `bytes` and `size` describe its buffer.
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    let machine = (status == 0)
        .then(|| usize::try_from(bytes).ok())
        .flatten()?;
    let resident = crate::state::runtime_metrics::read_process_resident_memory_bytes()?;
    Some(machine.saturating_sub(usize::try_from(resident).ok()?))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_memory() -> Option<usize> {
    None
}
