//! Re-export of build config generation, which now lives in `aurcache-utils`
//! so the API can assemble a worker `JobDescriptor` without depending on the
//! Docker builder crate.
pub use aurcache_utils::job_config::{
    base_pacman_config, create_makepkg_config, create_pacman_config,
};
