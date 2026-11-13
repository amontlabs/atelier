//! Rules currency (DESIGN §3 `atelier rules`). Every command but `init`, `rules`, `--version` and
//! `status` verifies the generated files byte for byte and refuses (4) with
//! `rules are stale: run atelier rules`. The generator and the comparison come with module 3.7;
//! until then every tree counts as current.

use crate::exit::Result;
use crate::git::Checkout;
use crate::settings::Settings;

/// The refusal message for stale rules.
pub const STALE: &str = "rules are stale: run atelier rules";

/// Verifies the generated rules. Stub until module 3.7: always current.
pub fn verify(_checkout: &Checkout, _settings: &Settings) -> Result<()> {
    Ok(())
}
