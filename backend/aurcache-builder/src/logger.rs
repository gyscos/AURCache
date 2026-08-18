//! Re-export of the shared build logger, which now lives in `aurcache-utils`
//! so server-side ingest can append to `builds.output` without depending on the
//! Docker builder crate.
pub use aurcache_utils::build_logger::BuildLogger;
