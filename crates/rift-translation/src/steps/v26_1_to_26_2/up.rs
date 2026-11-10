//! A 26.2 client on a 26.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use crate::steps::Make;

pub const STEP: Option<Make> = None;
