//! Synchronised registries: the dimension codec of 1.16+ and configuration-phase registry data.
use super::HasShared;
use crate::step::RouteBuilder;

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    builder
}
