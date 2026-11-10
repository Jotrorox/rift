//! A 26.2 client on a 26.3 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use crate::steps::Make;

pub const STEP: Option<Make> = None;
