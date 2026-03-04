use chrono::{DateTime, Utc};
use circular_buffer::CircularBuffer;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A single message received from a Telegram broadcast channel.
#[derive(Clone, Debug)]
pub struct ChannelMessage {
    pub id: i32,
    pub text: String,
    pub date: DateTime<Utc>,
    pub channel_title: String,
    pub channel_id: i64,
}

/// A point-in-time snapshot of one channel's message buffer, cheaply cloned
/// out of the shared state for the LLM processing loop.
pub struct ChannelSnapshot {
    pub channel_id: i64,
    pub channel_title: String,
    /// Messages sorted oldest-first (iteration order of CircularBuffer).
    pub messages: Vec<ChannelMessage>,
}

/// Internal type: per-channel ring buffer, heap-allocated (512 messages max).
type ChannelRingBuffer = Box<CircularBuffer<512, ChannelMessage>>;

/// The shared concurrent store for all channel message buffers.
///
/// Writer: the Telegram update listener task (pushes new messages).
/// Reader: the LLM processing task (snapshots all buffers for prompt construction).
///
/// Uses `std::sync::RwLock` (not tokio) because critical sections are fast
/// synchronous operations (push or iterate+clone) — no `.await` inside the lock.
#[derive(Clone)]
pub struct ChannelBuffers {
    inner: Arc<RwLock<HashMap<i64, ChannelRingBuffer>>>,
}

impl ChannelBuffers {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Push a new message into the appropriate channel's ring buffer.
    /// If the buffer is full (512 messages), the oldest message is silently evicted.
    /// Creates a new buffer for previously-unseen channels.
    ///
    /// **Deduplication:** skips the push if a message with the same `id` already
    /// exists in that channel's buffer. This is needed because both the update
    /// stream and the `getChannelDifference` poll loop deliver the same messages.
    /// Scanning up to 512 `i32` IDs is negligible overhead.
    pub fn push(&self, msg: ChannelMessage) {
        let mut map = self.inner.write().expect(
            "channel buffer RwLock poisoned — a task panicked while holding the write lock; \
             this is a bug in the application",
        );
        let buffer = map
            .entry(msg.channel_id)
            .or_insert_with(CircularBuffer::boxed);
        if buffer.iter().any(|existing| existing.id == msg.id) {
            return;
        }
        buffer.push_back(msg);
    }

    /// Take a consistent point-in-time snapshot of all non-empty channel buffers.
    /// Clones messages out so the caller can process without holding any lock.
    pub fn snapshot(&self) -> Vec<ChannelSnapshot> {
        let map = self.inner.read().expect(
            "channel buffer RwLock poisoned — a task panicked while holding a lock; \
             this is a bug in the application",
        );
        map.iter()
            .filter(|(_, buf)| !buf.is_empty())
            .map(|(&channel_id, buf)| {
                let messages: Vec<ChannelMessage> = buf.iter().cloned().collect();
                let channel_title = messages
                    .first()
                    .map(|m| m.channel_title.clone())
                    .unwrap_or_default();
                ChannelSnapshot {
                    channel_id,
                    channel_title,
                    messages,
                }
            })
            .collect()
    }

    /// Number of distinct channels that have at least one buffered message.
    pub fn channel_count(&self) -> usize {
        self.inner
            .read()
            .expect("channel buffer RwLock poisoned")
            .len()
    }

    /// Total messages across all channel buffers.
    pub fn total_messages(&self) -> usize {
        self.inner
            .read()
            .expect("channel buffer RwLock poisoned")
            .values()
            .map(|buf| buf.len())
            .sum()
    }

    /// Returns `true` if a buffer exists for the given channel ID.
    pub fn has_channel(&self, channel_id: i64) -> bool {
        self.inner
            .read()
            .expect("channel buffer RwLock poisoned")
            .contains_key(&channel_id)
    }
}
