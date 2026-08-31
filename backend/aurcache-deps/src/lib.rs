mod client;
mod deps;
mod model;
pub mod paths;
mod repo;

pub use client::AurClient;
pub use deps::{deps_from_srcinfo, parse_dep};
pub use model::{DependencyResolution, Error, Package, PkgDeps};

/// Convert a search result into the API shape.
///
/// Implemented here rather than in the server: `Package` is defined in this
/// crate and `ApiPackage` in `aurcache-common`, so the server — owning
/// neither — cannot write this impl.
impl From<Package> for aurcache_common::api::aur::ApiPackage {
    fn from(package: Package) -> Self {
        Self {
            name: package.name,
            version: package.version,
            description: package.description,
        }
    }
}
