//! A 1.16.1 client on a 1.16.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use crate::steps::Make;

pub const STEP: Option<Make> = None;
