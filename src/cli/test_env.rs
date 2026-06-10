//! Shared helpers for tests that mutate `EPAZOTE_*` environment variables.
//!
//! Environment variables are process-global and cargo runs tests in parallel
//! threads, so every test module touching them must serialize on the SAME
//! mutex — separate per-module locks do not exclude each other and produce
//! flaky failures.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub const CLI_ENV_VARS: [&str; 5] = [
    "EPAZOTE_CONFIG",
    "EPAZOTE_PORT",
    "EPAZOTE_BIND",
    "EPAZOTE_VERBOSE",
    "EPAZOTE_JSON_LOGS",
];

/// Acquires the process-wide env lock, recovering from poisoning so one
/// panicked test does not cascade into every later env test.
pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct EnvVarGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    pub fn set(name: &'static str, value: Option<&OsStr>) -> Self {
        let previous = std::env::var_os(name);
        unsafe {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
        Self { name, previous }
    }

    pub fn clear(name: &'static str) -> Self {
        Self::set(name, None)
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(value) = &self.previous {
                std::env::set_var(self.name, value);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}

/// Locks the env and clears every CLI-related `EPAZOTE_*` variable; the
/// returned guards restore the previous values on drop.
pub fn lock_and_clear_cli_env() -> (MutexGuard<'static, ()>, [EnvVarGuard; 5]) {
    let lock = lock_env();
    let guards = CLI_ENV_VARS.map(EnvVarGuard::clear);
    (lock, guards)
}
