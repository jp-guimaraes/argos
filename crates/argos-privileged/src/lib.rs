//! Exposes the `argos-helper` <-> `argos` IPC contract as a library, so
//! `argos-cli` can build a [`protocol::WritePlan`] and parse [`protocol::Event`]
//! lines without duplicating their definition. Depending on this crate's
//! library target does **not** pull in anything privileged: the `argos-helper`
//! binary (`main.rs`) is the only thing here that ever runs as root, and Cargo
//! links a dependent only against the library target.

pub mod protocol;
