//! Readiness gate: the window opens at once while `App::start` runs in the background, and
//! every command, the file protocol and shutdown wait here until the App is ready.

use tokio::sync::watch;

/// Start-up phase of the backend.
#[derive(Debug)]
pub enum Phase<T> {
    /// `App::start` is still running.
    Starting,
    /// Started; commands go to this value.
    Ready(T),
    /// Start failed with this user-facing error.
    Failed(String),
}

/// Waits for the backend. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Gate<T> {
    rx: watch::Receiver<Phase<T>>,
}

/// Sets the phase; dropping it before `Ready` makes waiters fail.
pub type GateSender<T> = watch::Sender<Phase<T>>;

const GONE: &str = "Tincan stopped. Restart it.";

impl<T: Clone> Gate<T> {
    /// A gate in the `Starting` phase and the sender that opens it.
    pub fn new() -> (GateSender<T>, Self) {
        let (tx, rx) = watch::channel(Phase::Starting);
        (tx, Self { rx })
    }

    /// Waits until start-up finished. Returns the backend or the start error.
    pub async fn get(&self) -> Result<T, String> {
        let mut rx = self.rx.clone();
        let phase = rx.wait_for(|p| !matches!(p, Phase::Starting)).await.map_err(|_| GONE.to_string())?;
        match &*phase {
            Phase::Ready(v) => Ok(v.clone()),
            Phase::Failed(e) => Err(e.clone()),
            Phase::Starting => Err(GONE.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn waits_until_ready() {
        let (tx, gate) = Gate::<u32>::new();
        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move { gate.get().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        tx.send_replace(Phase::Ready(7));
        assert_eq!(waiter.await.unwrap(), Ok(7));
        // later callers do not wait, even after the sender is gone
        drop(tx);
        assert_eq!(gate.get().await, Ok(7));
    }

    #[tokio::test]
    async fn start_error_reaches_every_caller() {
        let (tx, gate) = Gate::<u32>::new();
        tx.send_replace(Phase::Failed("profile in use".into()));
        assert_eq!(gate.get().await, Err("profile in use".into()));
    }

    #[tokio::test]
    async fn dropped_sender_does_not_hang() {
        let (tx, gate) = Gate::<u32>::new();
        drop(tx);
        assert_eq!(gate.get().await, Err(GONE.to_string()));
    }
}
