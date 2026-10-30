//! Cancelling work that runs on its own thread.
//!
//! A token is scoped to a thread with [`with`], and the places that wait —
//! a child process, a poll loop, a copy — check [`requested`] and stop. So a
//! download can be cancelled without every backend taking the token as an
//! argument.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Set by one thread, checked by another.
#[derive(Clone, Debug, Default)]
pub struct Token(Arc<AtomicBool>);

impl Token {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Token>> = const { RefCell::new(None) };
}

/// Runs `f` with `token` as this thread's.
pub fn with<T>(token: Token, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<Token>);
    impl Drop for Reset {
        fn drop(&mut self) {
            CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(CURRENT.with(|c| c.borrow_mut().replace(token)));
    f()
}

/// Whether this thread's work has been cancelled.
pub fn requested() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(Token::is_cancelled))
}

/// Sleeps for `d`, waking early when cancelled. True when it slept the whole time.
pub fn sleep(d: Duration) -> bool {
    const SLICE: Duration = Duration::from_millis(200);
    let until = Instant::now() + d;
    loop {
        if requested() {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(SLICE));
    }
}

/// A reader that fails once this thread's work is cancelled.
pub struct Reader<R>(pub R);

impl<R: std::io::Read> std::io::Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if requested() {
            return Err(std::io::Error::other("cancelled"));
        }
        self.0.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_token_reaches_only_its_own_thread_and_scope() {
        let token = Token::default();
        token.cancel();
        assert!(!requested());
        with(token.clone(), || {
            assert!(requested());
            let other = std::thread::spawn(requested).join().unwrap();
            assert!(!other, "another thread has no token");
        });
        assert!(!requested(), "the scope ends with `with`");
    }

    #[test]
    fn sleep_and_reads_stop_when_cancelled() {
        let token = Token::default();
        let started = Instant::now();
        let slept = with(token.clone(), || {
            let t = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                t.cancel();
            });
            sleep(Duration::from_secs(10))
        });
        assert!(!slept);
        assert!(started.elapsed() < Duration::from_secs(2));

        let err = with(token, || Reader(&b"abc"[..]).read(&mut [0; 3])).unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
    }
}
