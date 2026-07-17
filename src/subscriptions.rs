//! Server-push subscriptions over the JSON-RPC control socket.
//!
//! Scraping `get_stats` at 1 Hz misses sub-second link-state changes
//! (NAK bursts, quality drops, reconnects). A subscription lets the
//! client register interest in a topic and receive push events as
//! JSON-RPC notifications on the same Unix socket.
//!
//! Wire format (over the control socket):
//!
//! Client → server:
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"subscribe","params":{"topic":"stats"}}
//! ```
//! Server reply:
//! ```json
//! {"jsonrpc":"2.0","result":{"subscription_id":"sub-0"},"id":1}
//! ```
//! Server push (whenever the topic produces an event):
//! ```json
//! {"jsonrpc":"2.0","method":"stats.update",
//!  "params":{"subscription_id":"sub-0","data":{ ... }}}
//! ```
//!
//! Topics currently published:
//!
//! - `stats` — per-link snapshot, fired once per second alongside the
//!   existing `get_stats` update.
//! - `priority.window` — fired on each critical-window extension from
//!   the priority sidecar (encoder keyframe hint).

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

/// One registered subscription. The sender is the *connection's* push
/// channel — all subscriptions multiplex over a single mpsc to the
/// client, with the subscription id carried in each notification so
/// clients can demux.
struct Entry {
    id: String,
    topic: String,
    sender: mpsc::Sender<String>,
}

/// Shared fan-out hub. Cheap to clone.
#[derive(Clone, Default)]
pub struct SubscriptionHub {
    /// Only the control socket hands out subscription ids, and that socket is
    /// Unix-domain. On other platforms the hub still publishes -- to nobody,
    /// since there is no way to subscribe.
    #[cfg(unix)]
    next_id: Arc<AtomicU64>,
    entries: Arc<Mutex<Vec<Entry>>>,
}

impl SubscriptionHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a subscription. Returns the subscription id the client
    /// should use to unsubscribe. Caller supplies their push channel;
    /// every published event on the topic is written to it.
    #[cfg(unix)]
    pub async fn subscribe(&self, topic: &str, push_tx: mpsc::Sender<String>) -> String {
        let id = format!("sub-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.entries.lock().await.push(Entry {
            id: id.clone(),
            topic: topic.to_string(),
            sender: push_tx,
        });
        id
    }

    /// Remove a subscription by id. Returns true if it was present.
    #[cfg(unix)]
    pub async fn unsubscribe(&self, id: &str) -> bool {
        let mut entries = self.entries.lock().await;
        let before = entries.len();
        entries.retain(|e| e.id != id);
        before != entries.len()
    }

    /// Fan-out a published event to every subscription of `topic`.
    /// Full-channel pushes are dropped — a backed-up subscriber never
    /// blocks the producer. Closed channels are cleaned up lazily next
    /// time we iterate.
    pub async fn publish(&self, topic: &str, data: Value) {
        let mut to_prune = Vec::new();
        {
            let entries = self.entries.lock().await;
            for entry in entries.iter() {
                if entry.topic != topic {
                    continue;
                }
                let envelope = json!({
                    "jsonrpc": "2.0",
                    "method": format!("{topic}.update"),
                    "params": {
                        "subscription_id": entry.id,
                        "data": data,
                    },
                });
                let line = match serde_json::to_string(&envelope) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                match entry.sender.try_send(line) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!(id = %entry.id, topic, "subscription channel full, dropped event");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        to_prune.push(entry.id.clone());
                    }
                }
            }
        }
        if !to_prune.is_empty() {
            let mut entries = self.entries.lock().await;
            entries.retain(|e| !to_prune.contains(&e.id));
        }
    }

    /// Number of active subscriptions (all topics combined). Exposed for
    /// telemetry; not needed for correctness.
    #[cfg(unix)]
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    #[allow(dead_code)] // Paired with `len()` for clippy; not called in-tree yet.
    pub async fn is_empty(&self) -> bool {
        self.entries.lock().await.is_empty()
    }
}

// Every test here drives `subscribe`/`unsubscribe`, which are unix-gated with
// the control socket that owns them, so the module is too — `cargo test` must
// keep compiling on Windows.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribe_publish_roundtrip() {
        let hub = SubscriptionHub::new();
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let id = hub.subscribe("stats", tx).await;
        assert!(id.starts_with("sub-"));

        hub.publish("stats", json!({"active_links": 3})).await;
        let line = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], "stats.update");
        assert_eq!(v["params"]["subscription_id"], id);
        assert_eq!(v["params"]["data"]["active_links"], 3);
    }

    #[tokio::test]
    async fn publish_respects_topic_filtering() {
        let hub = SubscriptionHub::new();
        let (tx1, mut rx1) = mpsc::channel::<String>(8);
        let (tx2, mut rx2) = mpsc::channel::<String>(8);
        hub.subscribe("stats", tx1).await;
        hub.subscribe("priority.window", tx2).await;

        hub.publish("stats", json!({})).await;
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());

        hub.publish("priority.window", json!({})).await;
        assert!(rx2.try_recv().is_ok());
        assert!(rx1.try_recv().is_err());
    }

    #[tokio::test]
    async fn unsubscribe_removes_entry() {
        let hub = SubscriptionHub::new();
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let id = hub.subscribe("stats", tx).await;
        assert!(hub.unsubscribe(&id).await);
        hub.publish("stats", json!({})).await;
        assert!(rx.try_recv().is_err());
        // Second unsubscribe is a no-op.
        assert!(!hub.unsubscribe(&id).await);
    }

    #[tokio::test]
    async fn dropped_channel_is_pruned_on_next_publish() {
        let hub = SubscriptionHub::new();
        {
            let (tx, _rx) = mpsc::channel::<String>(8);
            hub.subscribe("stats", tx).await;
        } // rx drops, closing the channel
        assert_eq!(hub.len().await, 1);
        hub.publish("stats", json!({})).await;
        assert_eq!(hub.len().await, 0);
    }
}
