use std::time::Instant;

// Define the Timer struct to measure elapsed time
pub struct Timer {
    start: Instant,
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

impl Timer {
    /// Creates a new timer with the specified label.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }
}

// Define the TimerManager struct to manage timers.
#[derive(Debug, Clone, Default)]
pub struct TimerManager;

impl TimerManager {
    /// Starts a new timer with the specified label.
    pub fn start(&self) -> Timer {
        Timer::new()
    }
}

// Define the global arguments
#[derive(Debug, Clone, Default)]
pub struct GlobalArgs {
    pub timer: TimerManager,
}

impl GlobalArgs {
    #[must_use]
    pub fn new() -> Self {
        Self {
            timer: TimerManager,
        }
    }
}
