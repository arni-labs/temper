//! Host and nested cgroup memory leave the tightest shared allowance.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

struct Host(BTreeMap<String, String>);

impl Host {
    fn new(groups: &str) -> Self {
        Self(BTreeMap::from([
            ("/proc/meminfo".into(), "MemAvailable: 1000 kB\n".into()),
            ("/proc/self/cgroup".into(), groups.into()),
        ]))
    }

    fn group(&mut self, path: &str, v2: bool, limit: &str, usage: usize, inactive: usize) {
        let (limit_file, usage_file, stat) = if v2 {
            ("memory.max", "memory.current", "inactive_file")
        } else {
            (
                "memory.limit_in_bytes",
                "memory.usage_in_bytes",
                "total_inactive_file",
            )
        };
        for (file, text) in [
            (limit_file, limit.to_string()),
            (usage_file, usage.to_string()),
            ("memory.stat", format!("{stat} {inactive}\n")),
        ] {
            self.0.insert(format!("{path}/{file}"), text);
        }
    }

    fn free(&self) -> Option<usize> {
        linux_free_memory(&|path: &Path| self.0.get(path.to_str()?).cloned())
    }
}

#[test]
fn v1_uses_the_process_memory_controller_and_every_ancestor() {
    // Memory need not be the only controller or the first hierarchy.
    let mut host = Host::new("5:cpu,cpuacct:/other\n7:memory,devices:/tenant/app\n");
    host.group("/sys/fs/cgroup/memory", false, "1000000", 100, 0);
    host.group(
        "/sys/fs/cgroup/memory/tenant",
        false,
        "600000",
        500000,
        10000,
    );
    host.group(
        "/sys/fs/cgroup/memory/tenant/app",
        false,
        "200000",
        150000,
        20000,
    );
    assert_eq!(
        host.free(),
        Some(70000),
        "child is tighter than parent and host"
    );
    host.group(
        "/sys/fs/cgroup/memory/tenant",
        false,
        "600000",
        590000,
        10000,
    );
    assert_eq!(
        host.free(),
        Some(20000),
        "siblings can make the parent tighter"
    );
}

#[test]
fn v2_uses_ancestors_and_skips_unlimited_groups() {
    let mut host = Host::new("0::/tenant/app\n");
    host.group("/sys/fs/cgroup", true, "max", 100, 0);
    host.group("/sys/fs/cgroup/tenant", true, "600000", 580000, 10000);
    host.group("/sys/fs/cgroup/tenant/app", true, "max", 150000, 20000);
    assert_eq!(host.free(), Some(30000));
}

#[test]
fn host_availability_and_exhausted_groups_bound_the_allowance() {
    let mut host = Host::new("7:memory:/\n");
    host.group("/sys/fs/cgroup/memory", false, "9000000", 1, 0);
    assert_eq!(host.free(), Some(1000 * 1024));
    host.group("/sys/fs/cgroup/memory", false, "100", 200, 50);
    assert_eq!(host.free(), Some(0));
}
