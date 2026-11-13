//! TERM, INT and HUP during a run (r4 §3.1): the handler only records the signal; the run loop
//! stops its current process group, writes a result that says so, and the command exits.
//!
//! Handlers answer only while a run is active, that is while a [`RunSignals`] guard lives. Outside
//! runs (before the first, and after the last has ended) each of these signals does what it did
//! before atelier installed anything: the default action ends the process, and a signal that was
//! ignored when atelier started (HUP under `nohup`) stays ignored and is never handled. A signal
//! received during a run stays recorded for the rest of the process: [`received`] reports it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The signals a run answers.
pub const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

struct Installed {
    received: Arc<AtomicUsize>,
    /// True while no run is active: the default action runs.
    outside: Arc<AtomicBool>,
    active: usize,
}

static STATE: Mutex<Option<Installed>> = Mutex::new(None);

/// Keeps the stop-signal handlers answering while it lives; see the module documentation.
#[derive(Debug)]
pub struct RunSignals(());

impl Drop for RunSignals {
    fn drop(&mut self) {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = state.as_mut() {
            s.active = s.active.saturating_sub(1);
            if s.active == 0 {
                s.outside.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Starts answering the stop signals until the guard is dropped. Installs the handlers on the
/// first call.
pub fn install() -> std::io::Result<RunSignals> {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.is_none() {
        let received = Arc::new(AtomicUsize::new(0));
        let outside = Arc::new(AtomicBool::new(false));
        for sig in STOP_SIGNALS {
            if ignored(sig) {
                continue;
            }
            signal_hook::flag::register_conditional_default(sig, Arc::clone(&outside))?;
            signal_hook::flag::register_usize(sig, Arc::clone(&received), sig as usize)?;
        }
        *state = Some(Installed {
            received,
            outside,
            active: 0,
        });
    }
    let s = state.as_mut().expect("installed above");
    s.active += 1;
    s.outside.store(false, Ordering::SeqCst);
    Ok(RunSignals(()))
}

/// True when the signal's disposition is "ignore".
fn ignored(sig: i32) -> bool {
    // SAFETY: a query only: a null new action, and `old` is a valid out pointer.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN
    }
}

/// The last stop signal received during a run, if any.
pub fn received() -> Option<i32> {
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state
        .as_ref()
        .map(|s| s.received.load(Ordering::SeqCst))
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
