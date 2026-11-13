//! Preflight (DESIGN §3, r4 §3.1, §6): every command first checks, in this order, that it runs in
//! an atelier project, takes `state.lock` shared, compares its own version with the pin, loads
//! the settings, and verifies the rules. Commands that use the versioned layer also require being
//! the version pinned on the integration branch and open that layer (converting an older format).
//! Tool prerequisites (`check`, `merge`, `test`) belong to module 3.2.

use std::path::Path;

use crate::dispatch::{self, VERSION};
use crate::exit::{Failure, Result};
use crate::git::Checkout;
use crate::rules;
use crate::settings::{self, SETTINGS_PATH, Settings};
use crate::state::{self, CURRENT_FORMAT, Converter, State};

/// Converters of the versioned layer, `CONVERTERS[n - 1]` from format `n` to `n + 1`.
pub const CONVERTERS: &[Converter] = &[];

/// What a command needs checked.
#[derive(Debug, Clone, Copy, Default)]
pub struct Needs {
    /// Verify the generated rules (all commands but `init`, `rules`, `--version`, `status`).
    pub rules: bool,
    /// Use the versioned layer (`approve`, `merge`, `promote`, `clean`).
    pub versioned: bool,
}

/// What a passed preflight hands to the command. `state` holds `state.lock` shared until dropped.
#[derive(Debug)]
pub struct Context {
    pub checkout: Checkout,
    pub state: State,
    pub settings: Settings,
}

/// The checkout and the raw settings text, or a refusal outside a project.
pub fn locate(cwd: &Path) -> Result<(Checkout, String)> {
    let checkout = Checkout::find(cwd)?
        .ok_or_else(|| Failure::refused(format!("{} is not in a git worktree", cwd.display())))?;
    let path = checkout.root.join(SETTINGS_PATH);
    if !path.is_file() {
        return Err(Failure::refused(format!(
            "not in an atelier project: {} has no {SETTINGS_PATH}",
            checkout.root.display()
        )));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {e}")))?;
    Ok((checkout, text))
}

/// The version check: this program's version must equal the pin.
pub fn check_version(pin: &str) -> Result<()> {
    if pin == VERSION {
        Ok(())
    } else {
        Err(Failure::refused(format!(
            "this checkout pins atelier {pin}; this program is {VERSION}. Run it as `atelier` (the dispatcher on PATH); if {pin} is missing, install it with:\n  {}",
            dispatch::install_command(pin)
        )))
    }
}

/// Runs the preflight of a command named `command`.
pub fn run(cwd: &Path, command: &str, needs: Needs) -> Result<Context> {
    let (checkout, text) = locate(cwd)?;
    let state = State::open(&checkout)?;
    check_version(&settings::read_pin(&text)?)?;
    let settings = Settings::load(&checkout.root)?;
    if needs.rules {
        rules::verify(&checkout, &settings)?;
    }
    if needs.versioned {
        require_main_version(&checkout, &settings, command)?;
        state.versioned(CURRENT_FORMAT, CONVERTERS)?;
    }
    Ok(Context {
        checkout,
        state,
        settings,
    })
}

/// Only the version pinned on the integration branch may use the versioned layer.
pub fn require_main_version(checkout: &Checkout, settings: &Settings, command: &str) -> Result<()> {
    let branch = &settings.integration_branch;
    match state::pin_on_integration_branch(checkout, branch)? {
        Some(pin) if pin == VERSION => Ok(()),
        Some(pin) => Err(Failure::refused(format!(
            "{command} runs only with the atelier version pinned on {branch} ({pin}); this is {VERSION}"
        ))),
        None => Err(Failure::refused(format!(
            "{command} needs {SETTINGS_PATH} on {branch}, which has none yet"
        ))),
    }
}

/// One line of the preflight report of `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub ok: bool,
    pub what: &'static str,
    pub detail: String,
}

/// The preflight as `status` reports it: every check, none stopping the others, nothing
/// converted. Also returns the shared state lock when it could be taken, held by the caller.
pub fn report(cwd: &Path) -> (Vec<Line>, Option<State>) {
    let mut lines = Vec::new();
    let mut push =
        |ok: bool, what: &'static str, detail: String| lines.push(Line { ok, what, detail });
    let (checkout, text) = match locate(cwd) {
        Ok(x) => x,
        Err(e) => {
            push(false, "project", e.message);
            return (lines, None);
        }
    };
    push(true, "project", checkout.root.display().to_string());
    let state = match State::open(&checkout) {
        Ok(s) => {
            push(true, "state", s.root().display().to_string());
            Some(s)
        }
        Err(e) => {
            push(false, "state", e.message);
            None
        }
    };
    match settings::read_pin(&text).and_then(|p| check_version(&p).map(|()| p)) {
        Ok(p) => push(true, "version", p),
        Err(e) => push(false, "version", e.message),
    }
    let settings = match Settings::load(&checkout.root) {
        Ok(s) => {
            push(true, "settings", SETTINGS_PATH.to_owned());
            Some(s)
        }
        Err(e) => {
            push(false, "settings", e.message);
            None
        }
    };
    if let Some(settings) = &settings {
        match rules::verify(&checkout, settings) {
            Ok(()) => push(true, "rules", "current".to_owned()),
            Err(e) => push(false, "rules", e.message),
        }
        if let Some(state) = &state {
            if require_main_version(&checkout, settings, "the versioned state").is_ok() {
                match state.format_line(CURRENT_FORMAT) {
                    Ok(()) => push(true, "state format", CURRENT_FORMAT.to_string()),
                    Err(m) => push(false, "state format", m),
                }
            }
        }
    }
    (lines, state)
}
