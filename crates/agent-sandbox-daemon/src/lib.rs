//! In-guest control daemon for tb-sandbox.
//!
//! Replaces the request-file-over-virtiofs transport with a multiplexed
//! connection: process exec with live stdout/stderr streaming, PTY sessions
//! with resize, binary-safe filesystem operations, git, and a health/version
//! endpoint. The host SDK speaks to it through libkrun's vsock-to-unix bridge.
//!
//! The server is transport-generic ([`server::handle_connection`] takes any
//! `AsyncRead + AsyncWrite`), so the exact same code path is exercised over an
//! `AF_UNIX` socket in tests and over `AF_VSOCK` in the guest.

pub mod fsops;
pub mod gitops;
pub mod proc;
pub mod server;

#[cfg(target_os = "linux")]
pub mod vsock;

pub use server::{handle_connection, serve_unix, VERSION};

#[cfg(target_os = "linux")]
pub use vsock::serve_vsock;
