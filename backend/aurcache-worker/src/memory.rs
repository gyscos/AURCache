//! Peak memory of a build's process tree.
//!
//! A build is a tree, not a process: `makechrootpkg` runs `systemd-nspawn`,
//! which runs `makepkg`, which runs a compiler once per core. Asking how much
//! memory "the build" used means asking about all of them at once, and the
//! answer an operator wants -- would this package build on a smaller machine --
//! is the high-water mark of that total.
//!
//! Sampled rather than measured, and that is a trade rather than a necessity.
//!
//! A per-build cgroup would give an exact `memory.peak`, and cgroups need no
//! systemd -- `mkdir` a directory under `/sys/fs/cgroup`, write a pid into
//! `cgroup.procs`, read `memory.peak`. What stands in the way is narrower:
//!
//!  * `docker/nspawn-wrapper.sh` forces `--keep-unit`, so the build reuses the
//!    worker's own cgroup rather than getting one. Reading that cgroup would
//!    measure the worker and every concurrent build together.
//!  * Giving each build a cgroup means enabling the memory controller in
//!    `cgroup.subtree_control`, which cgroup v2 refuses while processes sit
//!    directly in that cgroup -- so the worker would first have to move itself
//!    into a leaf, restructuring the hierarchy it was handed.
//!  * That hierarchy is not always the worker's to restructure. A container
//!    needs a writable `/sys/fs/cgroup`; a native install runs under a systemd
//!    unit, where the subtree belongs to systemd unless the unit sets
//!    `Delegate=yes`.
//!
//! Sampling `/proc` needs none of that and behaves the same everywhere. The
//! cost is that a spike shorter than the interval is invisible: this is a floor
//! on what the build needed, not a bound.
//!
//! Prefers PSS over RSS. Summing RSS across a tree counts every shared page
//! once per process, and a build with eight parallel compilers sharing libc
//! reports memory it never used. PSS divides each shared page among the
//! processes mapping it, so the sum over a tree is the tree's real footprint.
//! `smaps_rollup` is unreadable for a process owned by another user without
//! privilege, so RSS remains the fallback.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How often the tree is measured. Frequent enough to catch a link step,
/// cheap enough not to matter: a few dozen small reads from `/proc`.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// A running measurement of one process tree's peak memory.
pub struct PeakMemory {
    peak_bytes: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl PeakMemory {
    /// Start sampling the tree rooted at `root_pid` until dropped.
    pub fn watching(root_pid: u32) -> Self {
        let peak_bytes = Arc::new(AtomicU64::new(0));
        let peak = Arc::clone(&peak_bytes);
        let task = tokio::spawn(async move {
            loop {
                // Blocking file reads, off the async threads: a build tree can
                // be a hundred processes and /proc reads are real syscalls.
                let root = root_pid;
                if let Ok(total) = tokio::task::spawn_blocking(move || tree_bytes(root)).await {
                    peak.fetch_max(total, Ordering::Relaxed);
                }
                tokio::time::sleep(SAMPLE_INTERVAL).await;
            }
        });
        Self { peak_bytes, task }
    }

    /// The high-water mark, or `None` if nothing was ever measured.
    ///
    /// `None` rather than zero: a tree that exited before the first sample, or
    /// one whose `/proc` entries could not be read, is unknown rather than
    /// empty, and the page shows those differently.
    #[must_use]
    pub fn peak(&self) -> Option<i64> {
        match self.peak_bytes.load(Ordering::Relaxed) {
            0 => None,
            n => i64::try_from(n).ok(),
        }
    }
}

impl Drop for PeakMemory {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Total memory of `root` and every process descended from it, in bytes.
fn tree_bytes(root: u32) -> u64 {
    let table = process_table();
    descendants(&table, root)
        .into_iter()
        .filter_map(process_bytes)
        .sum()
}

/// Every `(pid, ppid)` currently in `/proc`.
///
/// Read wholesale rather than walking `children` files: the build runs inside a
/// PID namespace of its own, and scanning the parent's view is what keeps the
/// tree visible from out here regardless of how nspawn arranged it.
fn process_table() -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            Some((pid, parse_ppid(&stat)?))
        })
        .collect()
}

/// The parent pid from a `/proc/<pid>/stat` line.
///
/// Parsed from after the last `)` rather than by splitting on spaces: field 2
/// is the executable name in parentheses and may itself contain spaces and
/// parentheses, so everything before the final one has to be skipped.
fn parse_ppid(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// `root` and everything descended from it, per the `(pid, ppid)` table.
fn descendants(table: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut found = vec![root];
    // Repeated passes rather than recursion: the table is unordered, so a
    // child can appear before its parent has been recognised as in the tree.
    loop {
        let before = found.len();
        for &(pid, ppid) in table {
            if found.contains(&ppid) && !found.contains(&pid) {
                found.push(pid);
            }
        }
        if found.len() == before {
            return found;
        }
    }
}

/// One process's memory, preferring PSS and falling back to RSS.
fn process_bytes(pid: u32) -> Option<u64> {
    let base = format!("/proc/{pid}");
    if let Ok(rollup) = std::fs::read_to_string(format!("{base}/smaps_rollup"))
        && let Some(kb) = parse_pss_kb(&rollup)
    {
        return Some(kb * 1024);
    }
    let statm = std::fs::read_to_string(format!("{base}/statm")).ok()?;
    parse_rss_pages(&statm).map(|pages| pages * page_size())
}

/// The `Pss:` line of a `smaps_rollup`, in kB.
fn parse_pss_kb(rollup: &str) -> Option<u64> {
    rollup
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The resident-pages field of a `/proc/<pid>/statm`.
fn parse_rss_pages(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` with a valid name is always safe to call.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(n).unwrap_or(4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command name is parenthesised and may contain spaces or its own
    /// parentheses, which is why the parser works backwards from the last one.
    #[test]
    fn ppid_survives_a_hostile_command_name() {
        assert_eq!(parse_ppid("42 (bash) S 7 42 42 0 -1 4194304").unwrap(), 7);
        assert_eq!(
            parse_ppid("42 (weird name (x) ) S 7 42 42 0 -1 4194304").unwrap(),
            7
        );
    }

    /// The whole tree, not just direct children: a compiler under makepkg under
    /// nspawn under makechrootpkg is four levels down and is most of the usage.
    #[test]
    fn descendants_reach_the_whole_tree() {
        //  10 -> 11 -> 12 -> 13, plus an unrelated 20.
        let table = [(11, 10), (12, 11), (13, 12), (20, 1), (21, 20)];
        let mut found = descendants(&table, 10);
        found.sort_unstable();
        assert_eq!(found, vec![10, 11, 12, 13]);
    }

    /// The table is in whatever order `/proc` yields, so a child listed before
    /// its parent must still be found.
    #[test]
    fn descendants_do_not_depend_on_table_order() {
        let table = [(13, 12), (12, 11), (11, 10)];
        let mut found = descendants(&table, 10);
        found.sort_unstable();
        assert_eq!(found, vec![10, 11, 12, 13]);
    }

    /// A process whose parent is not in the tree contributes nothing, or one
    /// busy worker would be blamed for the whole machine.
    #[test]
    fn descendants_exclude_unrelated_processes() {
        let table = [(11, 10), (20, 1)];
        assert_eq!(descendants(&table, 10), vec![10, 11]);
    }

    #[test]
    fn pss_is_read_in_kilobytes() {
        let rollup =
            "Rss:                1234 kB\nPss:                 567 kB\nShared_Clean: 0 kB\n";
        assert_eq!(parse_pss_kb(rollup).unwrap(), 567);
    }

    #[test]
    fn rss_comes_from_the_second_statm_field() {
        // size resident shared text lib data dt
        assert_eq!(parse_rss_pages("4096 512 128 1 0 256 0").unwrap(), 512);
    }

    /// Nothing sampled is unknown, not zero -- the page renders the two
    /// differently and "0 bytes" would be a claim rather than an absence.
    #[tokio::test]
    async fn an_unsampled_tree_reports_nothing() {
        let watcher = PeakMemory::watching(u32::MAX);
        assert!(watcher.peak().is_none());
    }
}
