//! Shared process-execution helpers for the tool adapters.

use std::io;

/// `ETXTBSY` — "text file busy". The errno the kernel returns when an
/// `exec` targets a file that is still open for writing. Same value (26)
/// on Linux and the BSDs, including macOS.
#[cfg(unix)]
const ETXTBSY: i32 = 26;

/// Total spawn attempts before an `ETXTBSY` is surfaced to the caller.
#[cfg(unix)]
const ETXTBSY_ATTEMPTS: u32 = 5;

/// Run `spawn`, retrying briefly when the OS reports the executable is
/// still open for writing (`ETXTBSY`).
///
/// togi writes a tool binary into its cache and can execute it moments
/// later. On unix a concurrent `fork` in another thread can inherit an
/// open write handle to that binary, so the `exec` races the write and
/// fails with `ETXTBSY` even though the file itself is complete. The
/// window is tiny, so a handful of short-backoff retries closes it. Any
/// other error, or success, returns immediately. On non-unix `ETXTBSY`
/// does not arise, so this is a single pass-through call.
pub(crate) fn retry_etxtbsy<T>(mut spawn: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    #[cfg(unix)]
    {
        let mut backoff = std::time::Duration::from_millis(5);
        for _ in 1..ETXTBSY_ATTEMPTS {
            match spawn() {
                Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(backoff);
                    backoff *= 2;
                }
                other => return other,
            }
        }
    }
    spawn()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn etxtbsy() -> io::Error {
        io::Error::from_raw_os_error(ETXTBSY)
    }

    #[test]
    fn succeeds_once_the_binary_is_no_longer_busy() {
        let calls = Cell::new(0);
        let result = retry_etxtbsy(|| {
            let n = calls.get() + 1;
            calls.set(n);
            if n < 3 { Err(etxtbsy()) } else { Ok(n) }
        });
        assert_eq!(result.expect("eventual success"), 3);
        assert_eq!(calls.get(), 3, "retried until the busy file settled");
    }

    #[test]
    fn gives_up_after_the_attempt_cap_and_returns_the_busy_error() {
        let calls = Cell::new(0);
        let result: io::Result<()> = retry_etxtbsy(|| {
            calls.set(calls.get() + 1);
            Err(etxtbsy())
        });
        let err = result.expect_err("never settles");
        assert_eq!(err.raw_os_error(), Some(ETXTBSY));
        assert_eq!(calls.get(), ETXTBSY_ATTEMPTS, "stops at the attempt cap");
    }

    #[test]
    fn other_errors_return_immediately_without_retrying() {
        let calls = Cell::new(0);
        let result: io::Result<()> = retry_etxtbsy(|| {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert_eq!(
            result.expect_err("propagated").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(calls.get(), 1, "only ETXTBSY is retried");
    }
}
