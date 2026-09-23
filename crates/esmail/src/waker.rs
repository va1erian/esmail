//! How background tasks tell a frontend that there is something to pump.
//!
//! Actors and forwarders push events into channels; the frontend drains them
//! on its own thread. A [`Waker`] is the only thing the core knows about that
//! thread: calling it must make the frontend run its next drain soon. Each
//! frontend builds one from its own primitive (egui's `request_repaint_of`, a
//! Win32 `PostMessage`, ...), so this module names no UI type.

use std::sync::Arc;

/// Wakes the frontend's event pump. Cheap to clone, callable from any thread,
/// and safe to call redundantly: implementations must coalesce.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A waker that does nothing, for headless use and tests.
pub fn noop() -> Waker {
    Arc::new(|| {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_waker_is_cloned_across_threads_and_called_concurrently() {
        const THREADS: usize = 8;
        const CALLS: usize = 1000;
        let count = Arc::new(AtomicUsize::new(0));
        let waker: Waker = {
            let count = Arc::clone(&count);
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            })
        };

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let waker = Arc::clone(&waker);
                std::thread::spawn(move || {
                    for _ in 0..CALLS {
                        waker();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(count.load(Ordering::Relaxed), THREADS * CALLS);
    }

    #[test]
    fn the_noop_waker_can_be_called() {
        noop()();
    }
}
