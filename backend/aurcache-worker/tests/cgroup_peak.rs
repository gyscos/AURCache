//! End-to-end check that a build's cgroup reports its peak memory.
//!
//! `#[ignore]` by default, and deliberately: [`Hierarchy::prepare`] moves every
//! process in the worker's own cgroup into a leaf, which on a developer's
//! machine means their login session. It is meant to be run inside a throwaway
//! privileged container:
//!
//! ```sh
//! cargo test -p aurcache-worker --test cgroup_peak --no-run
//! docker run --rm --privileged --cgroupns=private \
//!     -v "$PWD:/w" archlinux /w/backend/target/.../cgroup_peak --ignored
//! ```

use aurcache_worker::cgroup::{BuildCgroup, Hierarchy};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The whole mechanism: prepare the hierarchy, give a build a cgroup, have the
/// child put itself in it before exec, and read back what it used.
///
/// The child allocates ~256 MiB, so the figure has to be at least that and
/// nowhere near the machine's total -- a cgroup that measured the worker
/// instead of the build would report something quite different.
#[test]
#[ignore = "restructures the caller's cgroup; run inside a privileged container"]
fn a_builds_cgroup_reports_its_peak_memory() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let cgroup = hierarchy
        .for_build(4242)
        .expect("creating the build cgroup");

    let handle = cgroup.procs_handle().expect("opening cgroup.procs");
    let mut cmd = Command::new("/bin/sh");
    // Touch every page so the memory is resident, then exit.
    cmd.args([
        "-c",
        "a=$(head -c 268435456 /dev/zero | tr '\\0' 'x'); echo ${#a} >/dev/null",
    ]);
    // SAFETY: one write to a descriptor opened before the fork.
    unsafe {
        cmd.pre_exec(move || BuildCgroup::join_current_process(&handle));
    }
    let status = cmd.status().expect("running the child");
    assert!(status.success(), "child failed: {status}");

    let peak = cgroup.peak_bytes().expect("memory.peak should be readable");
    assert!(
        peak >= 200 * 1024 * 1024,
        "peak {peak} is below what the child allocated; the child was probably \
         never placed in the cgroup"
    );
    assert!(
        peak < 8 * 1024 * 1024 * 1024,
        "peak {peak} is implausibly large; this looks like the whole machine \
         rather than one build"
    );
}
