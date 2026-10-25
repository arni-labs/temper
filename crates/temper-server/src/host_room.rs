//! The memory this host leaves the joint walks of the specs it is sent.
//!
//! A joint walk holds every state it has seen, so a model that never ends
//! fills the host until the host kills the process, with every tenant on it.
//! The walks of one process share [`HOST_ROOM`]: together they hold at most
//! half of what the host leaves free, its memory limit less the server's own
//! resident memory. Half, because a walk's count of what it holds is an
//! estimate (the allocator rounds and fragments), and the server keeps the
//! rest to go on serving while a walk runs. Both are read again each time a
//! walk asks for more, so the room follows the server as it grows.

use std::sync::atomic::{AtomicUsize, Ordering};

use temper_verify::composite::Room;

use crate::state::runtime_metrics::read_process_resident_memory_bytes;

const MB: usize = 1 << 20;

/// The memory this process's joint walks hold.
pub struct HostRoom {
    held: AtomicUsize,
}

/// The one room the joint walks of this process share.
pub static HOST_ROOM: HostRoom = HostRoom {
    held: AtomicUsize::new(0),
};

impl HostRoom {
    /// The bytes the walks may hold together now, with the host's limit and
    /// the server's own resident memory it was worked out from.
    fn room(&self, held: usize) -> Result<(usize, usize, usize), String> {
        let limit = host_memory_limit()
            .ok_or_else(|| "this host's memory limit could not be read".to_string())?;
        let resident = read_process_resident_memory_bytes()
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| "this server's resident memory could not be read".to_string())?;
        let own = resident.saturating_sub(held);
        Ok((limit.saturating_sub(own) / 2, limit, own))
    }
}

impl Room for HostRoom {
    fn take(&self, bytes: usize) -> Result<(), String> {
        let mut held = self.held.load(Ordering::Relaxed);
        loop {
            let (room, limit, own) = self.room(held)?;
            let after = held.saturating_add(bytes);
            if after > room {
                return Err(format!(
                    "this host leaves joint walks {} MB (half of its {} MB less the server's own {} MB) \
                     and they hold {} MB",
                    room / MB,
                    limit / MB,
                    own / MB,
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

/// The memory this host gives the process: its cgroup's limit when it has
/// one, and never more than the machine has.
pub fn host_memory_limit() -> Option<usize> {
    let machine = machine_memory();
    match (cgroup_memory_limit(), machine) {
        (Some(cgroup), Some(machine)) => Some(cgroup.min(machine)),
        (cgroup, machine) => cgroup.or(machine),
    }
}

#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<usize> {
    // cgroup v2 says "max" when unlimited; v1 says a number near 2^63.
    let read = |path: &str| std::fs::read_to_string(path).ok(); // determinism-ok: host memory limit read
    if let Some(text) = read("/sys/fs/cgroup/memory.max") {
        return text.trim().parse().ok();
    }
    let limit: usize = read("/sys/fs/cgroup/memory/memory.limit_in_bytes")?
        .trim()
        .parse()
        .ok()?;
    (limit < 1 << 60).then_some(limit)
}

#[cfg(not(target_os = "linux"))]
fn cgroup_memory_limit() -> Option<usize> {
    None
}

#[cfg(target_os = "linux")]
fn machine_memory() -> Option<usize> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?; // determinism-ok: host memory read
    let line = info.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kb: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

#[cfg(target_os = "macos")]
fn machine_memory() -> Option<usize> {
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
    (status == 0).then(|| usize::try_from(bytes).ok()).flatten()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn machine_memory() -> Option<usize> {
    None
}
