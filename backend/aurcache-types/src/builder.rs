use aurcache_db::{builds, packages};

#[derive(Clone)]
pub enum Action {
    Build(Box<packages::Model>, Box<builds::Model>),
    Cancel(i32),
}

/// Values stored in the `builds.status` column.
pub use crate::build_state::{BuildState, BuildStates};
