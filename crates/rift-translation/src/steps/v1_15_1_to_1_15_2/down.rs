//! A 1.15.1 client on a 1.15.2 backend: the protocols differ only in their
//! number, so the shared codecs' defaults suffice.
use super::{NEWER, OLDER};
use crate::steps::v1_14_4_to_1_15::support::defaults_step;

defaults_step!(Down, OLDER, NEWER);
