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
