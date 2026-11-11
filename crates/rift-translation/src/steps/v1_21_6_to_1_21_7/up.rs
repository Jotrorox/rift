//! A 1.21.7 client on a 1.21.6 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::Renumber;
use crate::steps::Make;

pub const STEP: Option<Make> = Some(|| Box::new(Renumber { upgrade: true }));
