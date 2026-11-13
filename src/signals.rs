//! TERM, INT and HUP during a run (r4 §3.1): the handler only records the signal; the run loop
//! stops its current process group, writes a result that says so, and the command exits.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

static RECEIVED: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

/// The signals a run answers.
pub const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Installs the handlers once. Later calls do nothing.
pub fn install() -> std::io::Result<()> {
    if RECEIVED.get().is_some() {
        return Ok(());
    }
    let flag = Arc::new(AtomicUsize::new(0));
    for sig in STOP_SIGNALS {
        signal_hook::flag::register_usize(sig, Arc::clone(&flag), sig as usize)?;
    }
    let _ = RECEIVED.set(flag);
    Ok(())
}

/// The last stop signal received since [`install`], if any.
pub fn received() -> Option<i32> {
    RECEIVED
        .get()
        .map(|f| f.load(Ordering::SeqCst))
        .filter(|s| *s != 0)
        .map(|s| s as i32)
}

pub fn name(sig: i32) -> &'static str {
    match sig {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        libc::SIGKILL => "SIGKILL",
        _ => "signal",
    }
}
