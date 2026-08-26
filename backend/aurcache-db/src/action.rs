//! Build-queue messages.
//!
//! Lives here rather than in `aurcache-types` because it carries database
//! models: keeping it in the types crate forced anything that wanted an API
//! shape — including a browser frontend — to pull in sea-orm.

use crate::{builds, packages};

#[derive(Clone)]
pub enum Action {
    Build(Box<packages::Model>, Box<builds::Model>),
    Cancel(i32),
}
