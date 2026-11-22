//! Browser WebSocket connections: every message goes to all of them.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Arc<str>>,
}

impl Default for Hub {
    fn default() -> Self {
        // Small: every slot can hold a whole state message until it is overwritten.
        Hub { tx: broadcast::channel(16).0 }
    }
}

impl Hub {
    pub fn count(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    pub fn broadcast(&self, message: &Value) {
        if self.count() > 0 {
            let _ = self.tx.send(Arc::from(message.to_string()));
        }
    }
}
