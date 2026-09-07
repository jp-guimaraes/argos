//! UI-agnostic orchestration of Argos's write and verify flows.
//!
//! This crate is the seam between "what Argos does" and "how a user is asked
//! about it". It resolves and refuses devices, classifies images, runs the
//! preflight checks, builds the `Plan` that crosses the privilege boundary,
//! elevates `argos-helper`, and turns its event stream into something a front
//! end can render.
//!
//! What it deliberately does *not* do is talk to anyone: no `println!`, no
//! `stdin`, no progress bar, no widgets. Confirmation in particular stays with
//! each front end -- [`prepare_write`] hands back a [`WritePreview`] carrying
//! the facts a prompt needs as data, and `argos-cli` and a GUI format them
//! their own way while running the identical checks underneath.
//!
//! That is also the safety argument: there is one implementation of "may I
//! write to this disk?", not one per interface. A front end cannot weaken it
//! by forgetting a step, and `argos-helper` re-validates everything anyway
//! (see `argos_privileged::protocol::validate_refreshed_device`).

pub mod cancel;
pub mod elevate;
pub mod events;
pub mod format;
pub mod platform;
pub mod prepare;

pub use argos_core::error::{ArgosError, Result};
pub use cancel::Canceller;
pub use elevate::{spawn, Running};
pub use events::{EventSink, NoopSink, Outcome, SessionEvent};
pub use format::human_size;
pub use platform::{boxed_platform, current_platform};
pub use prepare::{
    canonicalize_iso_path, check_device_is_offerable, prepare_verify, prepare_write,
    PreparedVerify, PreparedWrite, SplitNote, VerifyRequest, WritePreview, WriteRequest,
};
