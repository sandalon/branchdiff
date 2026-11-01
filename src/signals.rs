use std::sync::atomic::{AtomicBool, Ordering};

static QUIT: AtomicBool = AtomicBool::new(false);

/// Turns SIGTERM and SIGHUP into a request to quit, so that the terminal is
/// restored the normal way. The handler only sets a flag, which is all that's
/// safe in one. Windows has neither signal.
pub fn catch_quit_signals() {
    #[cfg(unix)]
    {
        extern "C" fn request_quit(_: libc::c_int) {
            QUIT.store(true, Ordering::SeqCst);
        }
        let handler: extern "C" fn(libc::c_int) = request_quit;
        for signal in [libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: the handler only stores to an atomic.
            unsafe { libc::signal(signal, handler as libc::sighandler_t) };
        }
    }
}

pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// One test, because a signal's handling is shared by the whole process.
    #[test]
    fn terminate_and_hangup_ask_to_quit() {
        catch_quit_signals();
        for signal in [libc::SIGTERM, libc::SIGHUP] {
            QUIT.store(false, Ordering::SeqCst);
            assert!(!quit_requested());
            // SAFETY: only signals this process, and the handler is in place.
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            assert!(quit_requested(), "{signal}");
        }
    }
}
