//! The pool against a real kernel: loop devices, btrfs, simple quotas.
//!
//! These need root through `sudo -n` and make real mounts, so they only run
//! when asked to: `AURCACHE_POOL_TESTS=1 cargo test -p aurcache-chroot`. Their
//! images go under Cargo's target directory rather than `/tmp`, which is often
//! a size-limited tmpfs.

use aurcache_chroot::{Backing, Pool, PoolConfig};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const MIB: u64 = 1 << 20;

fn enabled() -> bool {
    if std::env::var_os("AURCACHE_POOL_TESTS").is_none() {
        eprintln!("skipping: set AURCACHE_POOL_TESTS=1 to run pool tests (needs sudo)");
        return false;
    }
    true
}

fn owner() -> (u32, u32) {
    // SAFETY: neither call has preconditions or can fail.
    unsafe { (libc::getuid(), libc::getgid()) }
}

fn sudo(args: &[&str]) -> std::process::Output {
    Command::new("sudo").arg("-n").args(args).output().unwrap()
}

fn sudo_ok(args: &[&str]) {
    let out = sudo(args);
    assert!(
        out.status.success(),
        "sudo {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A scratch directory for one test, removed (with anything mounted in it)
/// when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("pool-{name}"));
        let scratch = Self(dir);
        scratch.cleanup();
        std::fs::create_dir_all(&scratch.0).unwrap();
        scratch
    }

    fn cleanup(&self) {
        let dir = self.0.display().to_string();
        // Unmount anything under the directory, deepest first, then detach
        // loop devices backed by files in it.
        let script = format!(
            r#"for m in $(findmnt -rn -o TARGET | grep "^{dir}" | sort -r); do umount -l "$m"; done
               for f in {dir}/*.img; do [ -e "$f" ] && losetup -j "$f" | cut -d: -f1 | xargs -r -n1 losetup -d; done
               rm -rf "{dir}""#
        );
        let _ = sudo(&["sh", "-c", &script]);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn config(scratch: &Scratch, total: u64) -> PoolConfig {
    PoolConfig {
        backing: Backing::Image {
            path: scratch.0.join("pool.img"),
            reserve: false,
        },
        mountpoint: scratch.0.join("pool"),
        total,
        owner: owner(),
    }
}

/// Stand in for `mkarchroot`: a base subvolume with some content.
async fn make_base(pool: &Pool, megabytes: u64) {
    let root = pool.root();
    sudo_ok(&["btrfs", "subvolume", "create", root.to_str().unwrap()]);
    sudo_ok(&["mkdir", "-p", root.join("usr").to_str().unwrap()]);
    sudo_ok(&[
        "sh",
        "-c",
        &format!(
            "head -c {megabytes}M /dev/urandom > {}",
            root.join("usr/blob").display()
        ),
    ]);
    pool.charge_to_total(&root).await.unwrap();
}

/// `fallocate` as the worker, into a directory it owns. Returns the error.
fn fallocate(path: &Path, bytes: u64) -> Option<String> {
    let out = Command::new("fallocate")
        .args(["-l", &bytes.to_string()])
        .arg(path)
        .output()
        .unwrap();
    (!out.status.success()).then(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Write `bytes` of incompressible data as the worker; the error, if any.
fn write(path: &Path, bytes: u64) -> Option<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "head -c {bytes} /dev/urandom > '{}' && sync -f '{0}'",
            path.display()
        ))
        .output()
        .unwrap();
    (!out.status.success()).then(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
}

fn sync(pool: &Pool) {
    sudo_ok(&["btrfs", "filesystem", "sync", pool.path().to_str().unwrap()]);
}

#[tokio::test]
async fn a_build_is_held_to_its_limit_and_the_pool_to_its_total() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("limits");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();

    let error = pool.lease(1, Some(64 * MIB)).await.unwrap_err();
    assert!(format!("{error:#}").contains("no base chroot"), "{error:#}");

    make_base(&pool, 100).await;
    sync(&pool);
    let total_before = pool.total_usage().unwrap().used;
    assert!(
        total_before >= 100 * MIB,
        "the base counts against the total"
    );

    // One build, limited to 64M.
    let build = pool.lease(1, Some(64 * MIB)).await.unwrap();
    assert_eq!(build.label(), "job-1");
    assert!(
        build.chroot().join("usr/blob").exists(),
        "a snapshot of the base"
    );
    sync(&pool);
    let fresh = pool.usage(build.group()).unwrap();
    assert!(
        fresh.used < MIB,
        "a fresh snapshot is charged ~nothing: {fresh:?}"
    );
    assert_eq!(fresh.limit, Some(64 * MIB));

    let refused = fallocate(&build.data().join("big"), 256 * MIB);
    assert!(
        refused.as_deref().is_some_and(|e| e.contains("quota")),
        "fallocate past the limit: {refused:?}"
    );
    let refused = write(&build.data().join("big"), 100 * MIB);
    assert!(refused.is_some(), "a write past the limit must fail");
    sync(&pool);
    assert!(pool.usage(build.group()).unwrap().at_limit(2 * MIB));

    // A second, unlimited build still stops at the pool's total.
    let other = pool.lease(2, None).await.unwrap();
    let refused = write(&other.data().join("big"), 700 * MIB);
    assert!(refused.is_some(), "the total must bind an unlimited build");
    sync(&pool);
    let total = pool.total_usage().unwrap();
    assert!(total.at_limit(4 * MIB), "{total:?}");

    // Releasing gives the space back, once the cleaner has run.
    pool.release(other).await;
    pool.release(build).await;
    assert!(!pool.path().join("job-1").exists());
    assert!(!pool.path().join("job-2.data").exists());
    sudo_ok(&["btrfs", "subvolume", "sync", pool.path().to_str().unwrap()]);
    sync(&pool);
    let after = pool.total_usage().unwrap().used;
    assert!(
        after < total_before + 4 * MIB,
        "the total drops back to about the base: {after} vs {total_before}"
    );

    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_sweep_clears_what_a_killed_worker_left() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("sweep");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 10).await;

    let running = pool.lease(7, Some(64 * MIB)).await.unwrap();
    let crashed = pool.lease(8, Some(64 * MIB)).await.unwrap();
    // A worker killed mid-build: the volumes are never released.
    std::mem::forget(crashed);

    let cleared = pool.sweep(&HashSet::from([7])).await;
    assert_eq!(cleared, 1);
    assert!(
        pool.path().join("job-7").exists(),
        "a running build is kept"
    );
    assert!(!pool.path().join("job-8").exists());
    assert!(!pool.path().join("job-8.data").exists());

    pool.release(running).await;
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn reopening_finds_the_same_pool() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("reopen");
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 10).await;
    let marker = pool.path().join("kept");
    std::fs::write(&marker, "still here").unwrap();

    // Still mounted: a worker restart.
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    assert!(marker.exists());

    // Unmounted and detached: a reboot. No mkfs this time.
    pool.unmount().await.unwrap();
    let pool = Pool::open(config(&scratch, 800 * MIB)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "still here");
    assert_eq!(pool.total_usage().unwrap().limit, Some(800 * MIB));
    pool.unmount().await.unwrap();
}

#[tokio::test]
async fn a_device_with_someone_elses_filesystem_is_never_formatted() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("foreign");
    let image = scratch.0.join("foreign.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 * MIB)
        .unwrap();
    let device =
        String::from_utf8(sudo(&["losetup", "--find", "--show", image.to_str().unwrap()]).stdout)
            .unwrap()
            .trim()
            .to_string();
    sudo_ok(&["mkfs.ext4", "-q", &device]);

    let error = Pool::open(PoolConfig {
        backing: Backing::Device(PathBuf::from(&device)),
        mountpoint: scratch.0.join("pool"),
        total: 128 * MIB,
        owner: owner(),
    })
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("refusing"), "{error:#}");
    let still =
        String::from_utf8(sudo(&["blkid", "-o", "value", "-s", "TYPE", &device]).stdout).unwrap();
    assert_eq!(still.trim(), "ext4", "the device must be left as it was");
}

#[tokio::test]
async fn the_image_follows_the_total_both_ways() {
    if !enabled() {
        return;
    }
    let scratch = Scratch::new("resize");
    let image = scratch.0.join("pool.img");
    let len = || std::fs::metadata(&image).unwrap().len();
    let pool = Pool::open(config(&scratch, 600 * MIB)).await.unwrap();
    make_base(&pool, 50).await;
    let small = len();
    assert_eq!(small, aurcache_chroot::pool::size_for(600 * MIB));

    pool.set_total(4096 * MIB).await.unwrap();
    assert_eq!(len(), aurcache_chroot::pool::size_for(4096 * MIB), "grown");

    pool.set_total(600 * MIB).await.unwrap();
    assert_eq!(len(), small, "shrunk back: the base fits");
    assert_eq!(pool.total_usage().unwrap().limit, Some(600 * MIB));
    assert!(pool.root().join("usr/blob").exists(), "the data survived");
    assert!(
        pool.host_free().is_some(),
        "a sparse image reports the host's room"
    );
    pool.unmount().await.unwrap();
}
