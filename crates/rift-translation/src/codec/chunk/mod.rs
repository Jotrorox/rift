//! Chunks, light, block updates and block entities.
pub mod container;
pub mod light;
pub mod model;
pub mod storage;
#[cfg(test)]
mod tests;

use super::HasShared;
use crate::step::RouteBuilder;

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    builder
}
