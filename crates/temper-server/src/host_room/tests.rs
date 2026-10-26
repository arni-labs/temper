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
            (
                "/proc/self/mountinfo".into(),
                "30 25 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n\
                 31 25 0:27 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n"
                    .into(),
            ),
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

#[test]
fn v2_checks_own_group_and_ancestors_on_subtree_mounts() {
    let groups = "0::/slice/tenant/server\n";
    for (base, root, mount_root) in [
        ("/slice", "/sys/fs/cgroup", "/sys/fs/cgroup"),
        ("/slice", "/memory mount", "/memory\\040mount"),
        ("/", "/sys/fs/cgroup/slice", "/sys/fs/cgroup"),
    ] {
        let mut files = BTreeMap::new();
        for (suffix, limit, usage, cache) in [
            ("", "max", 0, 0),
            ("/tenant", "4096", 100, 50),
            ("/tenant/server", "300", 100, 20),
        ] {
            let dir = format!("{root}{suffix}");
            files.insert(Path::new(&dir).join("memory.max"), limit.into());
            files.insert(Path::new(&dir).join("memory.current"), usage.to_string());
            files.insert(
                Path::new(&dir).join("memory.stat"),
                format!("inactive_file {cache}\n"),
            );
        }
        let mounts = format!("30 20 0:1 {base} {mount_root} rw - cgroup2 cgroup rw\n");
        let read = |path: &Path| files.get(path).cloned();
        assert_eq!(cgroup_free(&read, groups, &mounts, 8000, true), 220);
        assert_eq!(cgroup_free(&read, groups, &mounts, 10, true), 10);
        // A shared ancestor can be tighter than the process's own group.
        files.insert(Path::new(root).join("tenant/memory.max"), "150".into());
        let read = |path: &Path| files.get(path).cloned();
        assert_eq!(cgroup_free(&read, groups, &mounts, 8000, true), 100);
        assert_eq!(
            cgroup_free(&read, "0::/other/server", &mounts, 8000, true),
            8000
        );
    }
}

#[test]
fn v1_checks_own_group_and_ancestors_on_a_subtree_mount() {
    let mut files = BTreeMap::new();
    for (dir, limit, usage, cache) in [
        ("/memory mount", 900, 100, 0),
        ("/memory mount/tenant", 500, 400, 50),
        ("/memory mount/tenant/server", 300, 100, 0),
    ] {
        files.insert(
            Path::new(dir).join("memory.limit_in_bytes"),
            limit.to_string(),
        );
        files.insert(
            Path::new(dir).join("memory.usage_in_bytes"),
            usage.to_string(),
        );
        files.insert(
            Path::new(dir).join("memory.stat"),
            format!("total_inactive_file {cache}\n"),
        );
    }
    let read = |path: &Path| files.get(path).cloned();
    let groups = "2:cpu:/elsewhere\n4:memory,other:/slice/tenant/server\n";
    let mounts = "30 20 0:1 /slice /memory\\040mount rw - cgroup cgroup rw,memory\n";
    assert_eq!(cgroup_free(&read, groups, mounts, 1000, false), 150);
    // The process's own group can be tighter than its shared ancestor.
    files.insert(
        Path::new("/memory mount/tenant/server/memory.limit_in_bytes").to_path_buf(),
        "120".into(),
    );
    let read = |path: &Path| files.get(path).cloned();
    assert_eq!(cgroup_free(&read, groups, mounts, 1000, false), 20);
    assert_eq!(cgroup_free(&read, groups, mounts, 10, false), 10);
}
