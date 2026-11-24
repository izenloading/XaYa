//! Crash points for recovery tests. In debug builds, `XAYA_AGENTD_CRASH_AT=<point>` makes the
//! service kill itself with SIGKILL when it reaches that point: no destructor, flush or reply
//! runs, exactly as when it is killed from outside. Release builds compile every point away.

/// Name of the environment variable naming the crash point; never passed to child processes.
pub(crate) const CRASH_ENV: &str = "XAYA_AGENTD_CRASH_AT";

#[inline]
pub(crate) fn crash_point(point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var_os(CRASH_ENV).is_some_and(|value| value == point) {
        // SAFETY: signalling this very process; SIGKILL cannot be caught.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
    #[cfg(not(debug_assertions))]
    let _ = point;
}

/// Base delay of the service's bounded retries. Debug builds let a test shorten it with
/// `XAYA_AGENTD_RETRY_BASE_MS`; release builds always use `default_ms`.
pub(crate) fn retry_base_ms(default_ms: u64) -> u64 {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("XAYA_AGENTD_RETRY_BASE_MS").ok().and_then(|ms| ms.parse().ok())
    {
        return ms;
    }
    default_ms
}
