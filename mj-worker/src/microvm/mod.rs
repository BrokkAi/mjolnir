//! `mj-worker microvm`: the host engine that runs OCI images as Nitro
//! Sandbox microVMs, and the guest agent that serves them.
//!
//! The engine is a Podman-compatible command line (`cli`) over a state
//! directory it alone owns (`state`). Images become ext4 disks (`image`),
//! volumes are ext4 disk files (`volume`), VMs are systemd transient units
//! running Firecracker (`vm`), and the engine reaches into a VM over the
//! vsock protocol (`protocol`, `client`) to a guest agent (`guest_agent`)
//! that runs as PID 1 from an initrd the engine builds (`boot`).

pub mod boot;
pub mod cli;
pub mod client;
pub mod guest_agent;
pub mod image;
pub mod net;
pub mod protocol;
pub mod replace_root;
pub mod state;
pub mod sys;
pub mod tarcopy;
pub mod template;
pub mod vm;
pub mod volume;

#[cfg(test)]
mod tests;

use std::ffi::OsString;

/// Entry point for `mj-worker microvm <args>`; returns the process exit status.
pub fn main(args: Vec<OsString>) -> anyhow::Result<i32> {
    cli::main(args)
}
