//! rovacom: behavior-preserving Rust port of the novacom client.
#![deny(unsafe_code)]

pub mod app;
pub mod cli;
pub mod devlist;
pub mod net;
pub mod packet;
pub mod session;
#[allow(unsafe_code)]
pub mod sys;
