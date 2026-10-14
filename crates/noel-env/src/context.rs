//! The context every operation takes (upstream Chord's `Context`, of which an
//! environment uses only the abort signal).

use tokio::sync::watch;

/// What an operation runs under. An operation checks its abort signal where
/// upstream's `NodeExecutionEnv` does and then fails with `aborted`; a
/// running command is killed. Dropping an operation's future also stops it.
#[derive(Debug, Clone, Default)]
pub struct Context {
    abort: Option<AbortSignal>,
}

impl Context {
    /// A context that is never aborted (upstream `BACKGROUND_CONTEXT`).
    pub fn background() -> Self {
        Self::default()
    }

    /// A context aborted by `signal` (upstream `withAbortSignal`).
    pub fn with_abort_signal(signal: AbortSignal) -> Self {
        Self {
            abort: Some(signal),
        }
    }

    pub fn abort_signal(&self) -> Option<&AbortSignal> {
        self.abort.as_ref()
    }

    pub fn is_aborted(&self) -> bool {
        self.abort.as_ref().is_some_and(AbortSignal::is_aborted)
    }

    /// Completes once the context is aborted; never without a signal.
    pub async fn aborted(&self) {
        match &self.abort {
            Some(signal) => signal.aborted().await,
            None => std::future::pending().await,
        }
    }
}

/// Aborts the contexts made from its signal.
#[derive(Debug)]
pub struct AbortController {
    sender: watch::Sender<bool>,
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortController {
    pub fn new() -> Self {
        Self {
            sender: watch::Sender::new(false),
        }
    }

    pub fn signal(&self) -> AbortSignal {
        AbortSignal {
            receiver: self.sender.subscribe(),
        }
    }

    pub fn abort(&self) {
        self.sender.send_replace(true);
    }
}

/// The signal of an [`AbortController`]. It stays aborted once aborted, also
/// after its controller is dropped.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    receiver: watch::Receiver<bool>,
}

impl AbortSignal {
    /// A signal that is aborted already.
    pub fn already_aborted() -> Self {
        let controller = AbortController::new();
        controller.abort();
        controller.signal()
    }

    pub fn is_aborted(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Completes once aborted. A signal whose controller was dropped without
    /// aborting never completes.
    pub async fn aborted(&self) {
        let mut receiver = self.receiver.clone();
        if receiver.wait_for(|aborted| *aborted).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_signal_reports_its_abort_also_after_the_controller_is_gone() {
        let controller = AbortController::new();
        let context = Context::with_abort_signal(controller.signal());
        assert!(!context.is_aborted());
        let waiting = tokio::spawn({
            let context = context.clone();
            async move { context.aborted().await }
        });
        controller.abort();
        drop(controller);
        waiting.await.unwrap();
        assert!(context.is_aborted());
        assert!(Context::with_abort_signal(AbortSignal::already_aborted()).is_aborted());
        assert!(!Context::background().is_aborted());
    }
}
