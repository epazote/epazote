//! Automated HTTP (microservices) supervisor.
//!
//! # Compatibility
//!
//! The stable, semver-covered surface of this crate is the `epazote`
//! **command line and its YAML configuration schema**. A release will not
//! remove a configuration key, change what one means, or break an existing
//! `epazote.yml`, outside of a major version.
//!
//! The **Rust API is not covered by semver.** `pub mod cli` exists so the
//! binary and the integration tests can share one implementation, not as a
//! library for downstream crates, and its types mirror the configuration
//! file directly. Every new configuration key therefore becomes a new `pub`
//! field on a `pub` struct - `cli::config::Action` gained `timeout` in 4.0.0
//! and `group` in 4.2.0 - which is a source-breaking change for anyone
//! constructing these types with an exhaustive struct literal.
//!
//! Requiring a major version for each new key would tie the configuration
//! format's release cadence to a Rust API that has no known consumers, so the
//! trade is made the other way round and stated here rather than left to be
//! discovered. Code depending on these types should pin an exact version.
//! `cli::config::Action`, the struct that has actually grown, implements
//! `Default`, so building it with `..Default::default()` keeps a later field
//! additive; the surrounding types do not, and have no such guarantee.

// epazote must be built with `panic = "unwind"`, which is the default.
//
// This is checked here rather than left as a comment on the release profile
// because two behaviours depend on it and neither can be covered by a test:
// tests never build with the release profile, so a change to it would reach a
// release unnoticed.
//
// - A panicking service task must not take the supervisor with it. Services
//   are supervised through `select_all`, whose `Err(JoinError)` arm reports
//   "A service monitoring task panicked". Unwinding contains the panic at the
//   task boundary; aborting kills the process, so one service's panic would
//   end monitoring for every other service and that arm would be dead code.
// - `Drop` must still run while unwinding, since that is what reaps a
//   timed-out fallback command through `kill_on_drop`.
//
// `panic = "abort"` is the usual next step when shrinking a release binary,
// and 4.1.0 already took the others - thin LTO, one codegen unit, stripping -
// so it is a plausible thing to reach for later without noticing the cost.
#[cfg(panic = "abort")]
compile_error!(
    "epazote requires panic=\"unwind\": a panicking service task must be \
     contained rather than abort the whole supervisor, and Drop must still \
     reap timed-out fallback commands via kill_on_drop"
);

pub mod cli;
