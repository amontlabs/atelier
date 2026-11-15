//! Preflight: every command first checks, in this order, that it runs in
//! an atelier project, takes `state.lock` shared, compares its own version with the pin, loads
//! the settings, and verifies the rules. Commands that use the versioned layer also require being
//! the version pinned on the integration branch, running in the main checkout while it is on that
//! branch (as the settings committed there name it), and open that layer (converting an older
//! format).
//! Tool prerequisites (`check`, `merge`, `test`) belong to `prereq`.

use std::path::Path;

use crate::dispatch::{self, VERSION};
use crate::exit::{Failure, Result};
use crate::git::Checkout;
use crate::names;
use crate::rules;
use crate::settings::{self, SETTINGS_PATH, Settings};
use crate::state::{self, CURRENT_FORMAT, Converter, MainBranch, State};

/// Converters of the versioned layer, `CONVERTERS[n - 1]` from format `n` to `n + 1`.
pub const CONVERTERS: &[Converter] = &[];

/// What a command needs checked.
#[derive(Debug, Clone, Copy, Default)]
pub struct Needs {
    /// Verify the generated rules (all commands but `init`, `rules`, `--version`, `status`).
    pub rules: bool,
    /// Use the versioned layer (`approve`, `merge`, `promote`, `clean`).
    pub versioned: bool,
    /// Refuse when the project's filesystem has less free space than `min_free_gb`.
    pub disk: bool,
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
    if needs.disk {
        require_free_disk(&checkout.root, settings.min_free_gb)?;
    }
    if needs.versioned {
        require_main_version(&checkout, command)?;
    }
    if needs.rules {
        rules::verify(&checkout, &settings)?;
    }
    if needs.versioned {
        state.versioned(CURRENT_FORMAT, CONVERTERS)?;
    }
    Ok(Context {
        checkout,
        state,
        settings,
    })
}

const GIB: u64 = 1 << 30;

/// Free bytes on the filesystem holding `path` (what an ordinary user can use).
pub fn free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// Refuses (4) when less than `min_free_gb` GB is free where `root` lives; 0 disables.
pub fn require_free_disk(root: &Path, min_free_gb: u64) -> Result<()> {
    if min_free_gb == 0 {
        return Ok(());
    }
    let free = free_bytes(root)
        .map_err(|e| Failure::refused(format!("free disk space of {}: {e}", root.display())))?;
    if free >= min_free_gb.saturating_mul(GIB) {
        return Ok(());
    }
    Err(Failure::refused(format!(
        "only {:.1} GB free where {} lives; min_free_gb is {min_free_gb}. Free space first: `atelier clean --yes`, `mbx gc`, or remove old worktrees",
        free as f64 / GIB as f64,
        root.display()
    )))
}

/// The integration branch and the version pinned on it, read from the main checkout: the
/// branch its `HEAD` names, the `integration_branch` committed there, and the pin committed on
/// that branch. Refuses (4) when any of them is missing.
pub fn integration_pin(checkout: &Checkout, command: &str) -> Result<(MainBranch, String, String)> {
    let main = MainBranch::read(checkout)?;
    let Some(head) = &main.head else {
        return Err(Failure::refused(format!(
            "{command} needs the main checkout on the integration branch; its HEAD is detached"
        )));
    };
    let Some(integration) = main.integration.clone() else {
        return Err(Failure::refused(format!(
            "{command} needs {SETTINGS_PATH} on {head}, which has none yet"
        )));
    };
    names::validate_branch(&checkout.root, &integration).map_err(|_| {
        Failure::refused(format!(
            "{head}:{SETTINGS_PATH}: integration_branch {integration:?} is not a valid branch name"
        ))
    })?;
    match state::pin_on_integration_branch(checkout, &integration)? {
        Some(pin) => Ok((main, integration, pin)),
        None => Err(Failure::refused(format!(
            "{command} needs {SETTINGS_PATH} on {integration}, which has none yet"
        ))),
    }
}

/// Only the version pinned on the integration branch may use the versioned layer, and only in
/// the main checkout while it is on that branch, the branch its own committed settings name.
/// The worktree's own settings play no part.
pub fn require_main_version(checkout: &Checkout, command: &str) -> Result<()> {
    let (main, branch, pin) = integration_pin(checkout, command)?;
    if pin != VERSION {
        return Err(Failure::refused(format!(
            "{command} runs only with the atelier version pinned on {branch} ({pin}); this is {VERSION}"
        )));
    }
    if !checkout.is_main_worktree() {
        return Err(Failure::refused(format!(
            "{command} runs only in the main checkout{}, not in a linked worktree",
            checkout
                .common_dir
                .parent()
                .map(|p| format!(" ({})", p.display()))
                .unwrap_or_default()
        )));
    }
    if main.head.as_deref() != Some(branch.as_str()) {
        return Err(Failure::refused(format!(
            "{command} runs only in the main checkout on {branch}; it is on {}",
            main.head.as_deref().unwrap_or("a detached HEAD")
        )));
    }
    Ok(())
}

/// One line of the preflight report of `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub ok: bool,
    pub what: &'static str,
    pub detail: String,
}

/// What `status` learns from the preflight: its lines, and the pieces that could be read. The
/// state lock is held shared while `state` lives.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<Line>,
    pub state: Option<State>,
    pub checkout: Option<Checkout>,
    pub settings: Option<Settings>,
    /// The version pinned on the integration branch matches this program: only then are the
    /// versioned records read.
    pub pinned_here: bool,
}

/// The preflight as `status` reports it: every check, none stopping the others, nothing
/// converted.
pub fn report(cwd: &Path) -> Report {
    let mut r = Report::default();
    let push = |r: &mut Report, ok: bool, what: &'static str, detail: String| {
        r.lines.push(Line { ok, what, detail })
    };
    let (checkout, text) = match locate(cwd) {
        Ok(x) => x,
        Err(e) => {
            push(&mut r, false, "project", e.message);
            return r;
        }
    };
    push(&mut r, true, "project", checkout.root.display().to_string());
    match State::open(&checkout) {
        Ok(s) => {
            push(&mut r, true, "state", s.root().display().to_string());
            r.state = Some(s);
        }
        Err(e) => push(&mut r, false, "state", e.message),
    }
    match settings::read_pin(&text).and_then(|p| check_version(&p).map(|()| p)) {
        Ok(p) => push(&mut r, true, "version", p),
        Err(e) => push(&mut r, false, "version", e.message),
    }
    match Settings::load(&checkout.root) {
        Ok(s) => {
            push(&mut r, true, "settings", SETTINGS_PATH.to_owned());
            r.settings = Some(s);
        }
        Err(e) => push(&mut r, false, "settings", e.message),
    }
    if let Some(settings) = r.settings.take() {
        match rules::verify(&checkout, &settings) {
            Ok(()) => push(&mut r, true, "rules", "current".to_owned()),
            Err(e) => push(&mut r, false, "rules", e.message),
        }
        match integration_pin(&checkout, "the versioned state") {
            Ok((_, branch, pin)) if pin == VERSION => {
                r.pinned_here = true;
                push(&mut r, true, "integration", format!("{branch} pins {pin}"));
            }
            Ok((_, branch, pin)) => push(
                &mut r,
                false,
                "integration",
                format!("{branch} pins {pin}; this is {VERSION}"),
            ),
            Err(e) => push(&mut r, false, "integration", e.message),
        }
        if r.pinned_here
            && let Some(state) = &r.state
        {
            match state.format_line(CURRENT_FORMAT) {
                Ok(()) => push(&mut r, true, "state format", CURRENT_FORMAT.to_string()),
                Err(m) => push(&mut r, false, "state format", m),
            }
        }
        match free_bytes(&checkout.root) {
            Ok(free) => {
                let gb = free as f64 / GIB as f64;
                let floor = settings.min_free_gb;
                push(
                    &mut r,
                    floor == 0 || free >= floor.saturating_mul(GIB),
                    "disk",
                    format!("{gb:.1} GB free (min_free_gb {floor})"),
                );
            }
            Err(e) => push(&mut r, false, "disk", e.to_string()),
        }
        r.settings = Some(settings);
    }
    r.checkout = Some(checkout);
    r
}
