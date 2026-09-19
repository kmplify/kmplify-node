//! Per-stream credit on the gateway link (protocol v4).
//!
//! Without it this node sends a relayed response as fast as the container
//! produces it, into a gateway that can only queue what its consumer has not
//! read yet. A video download on a phone connection is enough to grow that
//! queue until the gateway is killed for memory, and every other session on
//! it dies too.
//!
//! The rule is yamux's, in its smallest form. A v4 gateway names a window in
//! its `welcome`. Each stream starts with that many bytes of credit; sending a
//! chunk spends credit, and the gateway grants more (`credit` frames) as the
//! consumer actually drains. A stream with no credit waits, which pushes the
//! pressure back to where it belongs: the container's own socket.
//!
//! A gateway that names no window predates this, and nothing is paced.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Semaphore;

/// Largest payload per chunk frame, before base64. Small enough that many
/// streams interleave fairly on one socket, large enough that the per-frame
/// JSON cost is noise.
pub const CHUNK_BYTES: usize = 256 * 1024;

/// A stream that has waited this long for credit has no reader: the consumer
/// left and the gateway has nobody to drain for. Give the container its
/// connection back instead of holding it open forever.
pub const CREDIT_STALL: Duration = Duration::from_secs(120);

/// Never let a grant (however wrong) push a semaphore toward its ceiling.
const MAX_GRANT: usize = 64 * 1024 * 1024;

/// One item of a streamed request body, as the HTTP client wants it.
pub type BodyItem = Result<Vec<u8>, std::io::Error>;

struct Upload {
    tx: tokio::sync::mpsc::UnboundedSender<BodyItem>,
    /// Bytes handed to the channel that the container has not consumed yet.
    outstanding: usize,
    /// Consumed bytes not yet reported back as credit.
    ungranted: usize,
}

#[derive(Default)]
struct State {
    /// The connected gateway's window, or None when it does not pace.
    window: Option<usize>,
    streams: HashMap<String, Arc<Semaphore>>,
    /// req_id -> a streamed upload in flight.
    uploads: HashMap<String, Upload>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// A new gateway connection: adopt its window and end every stream of the
/// previous one. Closing the semaphores wakes their waiters with an error, so
/// a relay task blocked on credit from a socket that no longer exists ends
/// now rather than after `CREDIT_STALL`.
pub fn connected(window: Option<usize>) {
    if let Ok(mut s) = state().lock() {
        for sem in s.streams.values() {
            sem.close();
        }
        s.streams.clear();
        // Dropping the senders ends the bodies short, which fails those
        // requests at the container: half an upload is not an upload.
        s.uploads.clear();
        s.window = window.filter(|w| *w > 0);
    }
}

/// The window named by a `welcome` frame, if it is a usable number.
pub fn window_from_welcome(welcome: &serde_json::Value) -> Option<usize> {
    let w = welcome.get("window")?.as_u64()?;
    // Below one chunk a stream could never send anything; treat a gateway
    // that says so as one that does not pace rather than deadlock on it.
    (w as usize >= CHUNK_BYTES).then_some(w as usize)
}

/// The sending side of one stream's credit.
pub struct StreamCredit {
    id: String,
    sem: Option<Arc<Semaphore>>,
}

/// Why a send could not proceed.
#[derive(Debug, PartialEq, Eq)]
pub enum Stalled {
    /// No credit arrived within `CREDIT_STALL`.
    NoReader,
    /// The gateway connection this stream belonged to is gone.
    LinkGone,
}

/// Start pacing a stream. Against a gateway without a window this is free
/// and `acquire` always succeeds at once.
pub fn open(stream_id: &str) -> StreamCredit {
    let sem = state().lock().ok().and_then(|mut s| {
        let window = s.window?;
        let sem = Arc::new(Semaphore::new(window));
        s.streams.insert(stream_id.to_string(), sem.clone());
        Some(sem)
    });
    StreamCredit {
        id: stream_id.to_string(),
        sem,
    }
}

impl StreamCredit {
    /// Spend credit for a chunk of `n` bytes, waiting for a grant if needed.
    pub async fn acquire(&self, n: usize) -> Result<(), Stalled> {
        let Some(sem) = &self.sem else { return Ok(()) };
        let n = n.min(u32::MAX as usize) as u32;
        match tokio::time::timeout(CREDIT_STALL, sem.acquire_many(n)).await {
            Ok(Ok(permit)) => {
                // Spent, not borrowed: credit comes back only as a grant.
                permit.forget();
                Ok(())
            }
            Ok(Err(_)) => Err(Stalled::LinkGone),
            Err(_) => Err(Stalled::NoReader),
        }
    }
}

impl Drop for StreamCredit {
    fn drop(&mut self) {
        if self.sem.is_some() {
            if let Ok(mut s) = state().lock() {
                s.streams.remove(&self.id);
            }
        }
    }
}

/// A `credit` frame arrived: the consumer drained `bytes` of `stream_id`.
/// Grants for streams that already ended are normal (the last batch is in
/// flight when the stream closes) and ignored.
pub fn grant(stream_id: &str, bytes: u64) {
    let sem = state()
        .lock()
        .ok()
        .and_then(|s| s.streams.get(stream_id).cloned());
    if let Some(sem) = sem {
        sem.add_permits((bytes as usize).min(MAX_GRANT));
    }
}

/// A `stream_cancel` frame arrived: the consumer of `stream_id` is gone.
/// Closing the semaphore fails the sender's next `acquire`, which ends the
/// stream and hands the container its connection back.
pub fn cancel(stream_id: &str) {
    let sem = state()
        .lock()
        .ok()
        .and_then(|s| s.streams.get(stream_id).cloned());
    if let Some(sem) = sem {
        sem.close();
    }
}

// ----- streamed uploads (gateway -> container) ----------------------------
//
// The mirror image of the above. The gateway sends `http_req_chunk` frames
// under a window of credit; this side queues them for the HTTP client and
// reports bytes back as the CONTAINER actually consumes them, so a slow
// container paces the consumer's upload instead of filling this process.

/// Register an upload before its first chunk can arrive. Must be called from
/// the connection's read loop, not from a spawned task: the chunks follow the
/// `http` frame immediately and have to find the channel.
pub fn upload_open(req_id: &str) -> tokio::sync::mpsc::UnboundedReceiver<BodyItem> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    if let Ok(mut s) = state().lock() {
        s.uploads.insert(
            req_id.to_string(),
            Upload {
                tx,
                outstanding: 0,
                ungranted: 0,
            },
        );
    }
    rx
}

/// A chunk arrived. False when it was refused: unknown upload, or a gateway
/// that sends past its credit (then the upload is failed, not grown).
pub fn upload_push(req_id: &str, bytes: Vec<u8>) -> bool {
    let Ok(mut s) = state().lock() else {
        return false;
    };
    // Twice the window, the same bound the gateway applies to us. Without a
    // window (never the case for a gateway that streams uploads) one frame.
    let bound = s.window.map(|w| 2 * w).unwrap_or(16 * 1024 * 1024);
    let Some(up) = s.uploads.get_mut(req_id) else {
        return false;
    };
    if up.outstanding + bytes.len() > bound {
        let _ = up.tx.send(Err(std::io::Error::other(
            "the gateway sent past this upload's credit",
        )));
        s.uploads.remove(req_id);
        return false;
    }
    up.outstanding += bytes.len();
    up.tx.send(Ok(bytes)).is_ok()
}

/// The body is complete (or, with `abort`, will never be): end the stream.
pub fn upload_end(req_id: &str, abort: bool) {
    if let Ok(mut s) = state().lock() {
        if let Some(up) = s.uploads.remove(req_id) {
            if abort {
                // An error, not a clean end: the container must see a failed
                // request rather than a short body it might accept.
                let _ = up
                    .tx
                    .send(Err(std::io::Error::other("the upload was aborted")));
            }
        }
    }
}

/// The HTTP client took `n` bytes for the container. Returns a grant to send
/// to the gateway once half a window has been consumed.
pub fn upload_consumed(req_id: &str, n: usize) -> Option<u64> {
    let mut s = state().lock().ok()?;
    let threshold = s.window.map(|w| w / 2).unwrap_or(usize::MAX).max(1);
    let up = s.uploads.get_mut(req_id)?;
    up.outstanding = up.outstanding.saturating_sub(n);
    up.ungranted += n;
    if up.ungranted < threshold {
        return None;
    }
    let grant = up.ungranted as u64;
    up.ungranted = 0;
    Some(grant)
}

/// The credit state is process-global, so tests that call `connected` must
/// not interleave: one would close the other's streams mid-assertion.
#[cfg(test)]
pub(crate) static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The state is process-global, so these run as ONE test: a second test
    // calling `connected` concurrently would close this one's streams.
    #[tokio::test(start_paused = true)]
    async fn credit_paces_a_stream_and_a_grant_releases_it() {
        let _serial = TEST_LOCK.lock().await;
        // A gateway that names no window: nothing is paced, ever.
        connected(None);
        let free = open("free");
        for _ in 0..1000 {
            assert_eq!(free.acquire(CHUNK_BYTES).await, Ok(()));
        }
        drop(free);

        // Window of two chunks: two go out, the third waits for a grant.
        connected(Some(2 * CHUNK_BYTES));
        let s = open("s-1");
        assert_eq!(s.acquire(CHUNK_BYTES).await, Ok(()));
        assert_eq!(s.acquire(CHUNK_BYTES).await, Ok(()));
        let waiting = tokio::spawn(async move {
            let r = s.acquire(CHUNK_BYTES).await;
            (s, r)
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!waiting.is_finished(), "sent past its credit");
        grant("s-1", CHUNK_BYTES as u64);
        let (s, r) = waiting.await.unwrap();
        assert_eq!(r, Ok(()));

        // Nobody reading: the stream gives up instead of holding the
        // container's connection open for good.
        let started = tokio::time::Instant::now();
        assert_eq!(s.acquire(CHUNK_BYTES).await, Err(Stalled::NoReader));
        assert!(started.elapsed() >= CREDIT_STALL);

        // A new connection ends the old one's streams at once.
        let waiting = tokio::spawn(async move { s.acquire(CHUNK_BYTES).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        connected(Some(2 * CHUNK_BYTES));
        assert_eq!(waiting.await.unwrap(), Err(Stalled::LinkGone));

        // The consumer left: the sender learns at its next chunk, not after
        // the stall timeout.
        let gone = open("gone");
        cancel("gone");
        assert_eq!(gone.acquire(1).await, Err(Stalled::LinkGone));
        drop(gone);
        cancel("never-existed");

        // Grants for a stream that is gone, and absurd grants, are harmless.
        grant("s-1", 10);
        let big = open("big");
        grant("big", u64::MAX);
        assert_eq!(big.acquire(CHUNK_BYTES).await, Ok(()));
        drop(big);
        assert!(state().lock().unwrap().streams.is_empty());

        // ----- uploads, same global state, so same test ------------------
        connected(Some(4 * CHUNK_BYTES));
        let mut rx = upload_open("up-1");
        assert!(upload_push("up-1", vec![1u8; CHUNK_BYTES]));
        assert!(upload_push("up-1", vec![2u8; CHUNK_BYTES]));
        assert_eq!(rx.recv().await.unwrap().unwrap().len(), CHUNK_BYTES);
        // Credit is reported at half a window (two chunks), not per chunk.
        assert_eq!(upload_consumed("up-1", CHUNK_BYTES), None);
        assert_eq!(
            upload_consumed("up-1", CHUNK_BYTES),
            Some(2 * CHUNK_BYTES as u64)
        );
        // A clean end closes the stream after what was queued.
        upload_end("up-1", false);
        assert!(rx.recv().await.unwrap().is_ok());
        assert!(rx.recv().await.is_none());
        assert!(!upload_push("up-1", vec![0u8; 1]));

        // An aborted upload reaches the container as an ERROR, never as a
        // short body it might accept.
        let mut rx = upload_open("up-2");
        upload_end("up-2", true);
        assert!(rx.recv().await.unwrap().is_err());

        // A gateway that ignores its credit gets the upload failed, not
        // this process's memory.
        let mut rx = upload_open("up-3");
        let mut accepted = 0;
        while upload_push("up-3", vec![0u8; CHUNK_BYTES]) {
            accepted += 1;
            assert!(accepted <= 8, "queued past twice the window");
        }
        assert_eq!(accepted, 8);
        let mut last = None;
        while let Ok(item) = rx.try_recv() {
            last = Some(item);
        }
        assert!(last.unwrap().is_err());

        // A new connection ends uploads of the old one.
        let mut rx = upload_open("up-4");
        connected(None);
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn the_window_is_read_from_the_welcome_or_not_at_all() {
        assert_eq!(
            window_from_welcome(&json!({"type": "welcome", "protocol": 4, "window": 4194304})),
            Some(4194304)
        );
        // A pre-v4 gateway, and values no stream could live with.
        assert_eq!(window_from_welcome(&json!({"type": "welcome"})), None);
        assert_eq!(window_from_welcome(&json!({"window": 0})), None);
        assert_eq!(window_from_welcome(&json!({"window": 1024})), None);
        assert_eq!(window_from_welcome(&json!({"window": "4194304"})), None);
        assert_eq!(window_from_welcome(&json!({"window": -5})), None);
    }
}
