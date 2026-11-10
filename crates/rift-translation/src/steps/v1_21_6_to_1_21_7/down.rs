//! A 1.21.6 client on a 1.21.7 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::Renumber;
use crate::steps::Make;

pub const STEP: Option<Make> = Some(|| Box::new(Renumber { upgrade: false }));
