//! Configuration for the legacy container executor.
//!
//! These are the pre-worker environment variables, read under their original
//! names and with their original meanings, because the whole point of this
//! executor is that an existing deployment keeps behaving as it did.

use aurcache_worker_core::config::{CoreConfig, env_opt, env_parse};
use std::path::PathBuf;

/// Where a build directory lives, from two points of view.
///
/// In the single-container topology the worker writes into a path inside its
/// own filesystem, but the Docker daemon binds the *host* path — they are the
/// same directory reached by different routes, and confusing them yields a
/// container with an empty bind mount and no explanation.
#[derive(Clone, Debug)]
pub struct BuildDirs {
    /// Path as the Docker daemon sees it (`BUILD_ARTIFACT_DIR`).
    pub host: PathBuf,
    /// Path as this process sees it.
    pub local: PathBuf,
}

/// Fully-resolved configuration for the container executor.
#[derive(Clone, Debug)]
pub struct Config {
    /// Settings shared with every executor.
    pub core: CoreConfig,
    /// Image each build container is created from (`BUILDER_IMAGE`).
    pub builder_image: String,
    /// Shared build directory, host and local views.
    pub dirs: BuildDirs,
    /// CPU limit in milli-CPUs, `0` for unlimited (`CPU_LIMIT`).
    pub cpu_limit: u64,
    /// Memory limit in MB, negative for unlimited (`MEMORY_LIMIT`).
    pub memory_limit: i64,
}

/// The default builder image, matching the pre-worker default.
const DEFAULT_BUILDER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest";

impl Config {
    /// Build a [`Config`] from the process environment.
    ///
    /// Returns `None` when `BUILD_ARTIFACT_DIR` is unset: without a host path
    /// for the bind mount there is nothing this executor can do, and guessing
    /// one would produce containers that silently build into nowhere.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let host = env_opt("BUILD_ARTIFACT_DIR").map(PathBuf::from)?;
        let local = env_opt("BUILD_ARTIFACT_DIR_LOCAL")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("builds")
            });

        Some(Self {
            core: CoreConfig::from_env(),
            builder_image: env_opt("BUILDER_IMAGE")
                .unwrap_or_else(|| DEFAULT_BUILDER_IMAGE.to_string()),
            dirs: BuildDirs { host, local },
            cpu_limit: env_parse("CPU_LIMIT").unwrap_or(0),
            memory_limit: env_parse("MEMORY_LIMIT").unwrap_or(-1),
        })
    }

    /// Docker's `NanoCpus`, or `None` for unlimited.
    #[must_use]
    pub fn nano_cpus(&self) -> Option<i64> {
        (self.cpu_limit > 0).then(|| i64::try_from(self.cpu_limit).unwrap_or(i64::MAX) * 1_000_000)
    }

    /// Docker's `MemorySwap` in bytes, or `None` for unlimited.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<i64> {
        (self.memory_limit > 0).then(|| self.memory_limit * 1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cpu: u64, mem: i64) -> Config {
        Config {
            core: CoreConfig::from_env(),
            builder_image: DEFAULT_BUILDER_IMAGE.to_string(),
            dirs: BuildDirs {
                host: PathBuf::from("/h"),
                local: PathBuf::from("/l"),
            },
            cpu_limit: cpu,
            memory_limit: mem,
        }
    }

    /// The pre-worker defaults (`0` CPU, `-1` memory) meant "unlimited"; they
    /// must not be passed to Docker as a literal zero, which means something
    /// else entirely.
    #[test]
    fn zero_and_negative_limits_mean_unlimited() {
        assert_eq!(cfg(0, -1).nano_cpus(), None);
        assert_eq!(cfg(0, -1).memory_bytes(), None);
    }

    #[test]
    fn limits_convert_to_docker_units() {
        assert_eq!(cfg(2000, 512).nano_cpus(), Some(2_000_000_000));
        assert_eq!(cfg(2000, 512).memory_bytes(), Some(512 * 1024 * 1024));
    }
}
