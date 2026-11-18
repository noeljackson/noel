//! The harness clock (plan slice 4): injectable, so sleeps and their
//! deadlines are deterministic in tests and goldens, and every process
//! that opens the same storage can be made to use the same clock
//! (upstream `HarnessOptions.now`).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The clock a harness reads: epoch milliseconds.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl Clock {
    /// The system clock (the default).
    pub fn system() -> Clock {
        Clock(Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_millis() as u64)
                .unwrap_or_default()
        }))
    }

    /// A clock reading `now`, re-read at each call (a test's mutable
    /// clock: change the value the closure reads, and every deadline
    /// moves with it).
    pub fn injected(now: impl Fn() -> u64 + Send + Sync + 'static) -> Clock {
        Clock(Arc::new(now))
    }

    /// The time now, in epoch milliseconds.
    pub fn now(&self) -> u64 {
        (self.0)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Clock")
    }
}

impl Default for Clock {
    fn default() -> Self {
        Clock::system()
    }
}
