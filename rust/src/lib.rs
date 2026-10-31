//! rovacom: behavior-preserving Rust port of the novacom client.
#![deny(unsafe_code)]

pub mod app;
pub mod cli;
pub mod console;
pub mod control;
pub mod devlist;
pub mod net;
pub mod packet;
pub mod session;
pub mod stdio;
#[allow(unsafe_code)]
pub mod sys;
