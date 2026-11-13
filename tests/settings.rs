//! Settings, on the full Silo example copied verbatim from the design.

mod common;

use std::fs;

use atelier::exit::Exit;
use atelier::settings::{PlatformRun, Settings};
use common::*;

const SILO: &str = include_str!("fixtures/silo-atelier.toml");

/// Every file the Silo example names.
const NAMED: &[&str] = &[
    "app/SiloUI/Cargo.toml",
    ".atelier/hooks/private-build-file.sh",
    ".atelier/hooks/runtime.sh",
    ".atelier/hooks/api-regenerate.sh",
    "app/SiloUI/scripts/ci/frontend.sh",
    "app/SiloUI/scripts/ci/scripts.sh",
    "app/SiloUI/scripts/ci/links.sh",
    "app/SiloUI/scripts/ci/linux-prepare.sh",
    "app/SiloUI/scripts/ci/linux.sh",
];

fn silo_repo() -> Repo {
    let repo = Repo::new();
    for f in NAMED {
        repo.write(f, "#!/bin/sh\n");
    }
    repo.write(".atelier/atelier.toml", SILO);
    repo
}

#[test]
fn the_silo_example_parses_and_validates() {
    let repo = silo_repo();
    let s = Settings::load(&repo.root).unwrap();
    assert_eq!(s.atelier, "0.1.0");
    assert_eq!(s.integration_branch, "main");
    assert_eq!(s.workspaces, vec!["app/SiloUI"]);
    assert_eq!(s.promotion_wait_seconds, 300);
    assert_eq!(s.rust.as_ref().unwrap().nextest, "0.9.151");
    let mbx = s.prerequisites.mbx.as_ref().unwrap();
    assert_eq!(mbx.version, "1.30.0");
    assert_eq!(mbx.settings["target.seed"], toml::Value::Boolean(true));
    assert_eq!(
        mbx.settings["build_script_execution"],
        toml::Value::Boolean(false)
    );
    assert_eq!(s.prerequisites.tools.len(), 2);
    assert_eq!(
        s.prerequisites.tools[1].platforms.as_deref(),
        Some(&["macos-aarch64".to_owned()][..])
    );
    assert_eq!(s.check.prepare.len(), 3);
    assert_eq!(s.check.steps.len(), 4);
    assert!(
        s.check
            .steps
            .iter()
            .any(|st| st.name == "typecheck" && st.compile)
    );
    assert_eq!(
        s.merge.after[0].commit.as_deref(),
        Some("Regenerate API files")
    );
    assert_eq!(
        s.platforms["macos-aarch64"].run,
        PlatformRun::Named("check".into())
    );
    assert_eq!(
        s.platforms["linux-x86_64"].run,
        PlatformRun::Command(vec!["app/SiloUI/scripts/ci/linux.sh".into()])
    );
}

#[test]
fn every_named_path_must_exist() {
    for missing in NAMED {
        let repo = silo_repo();
        fs::remove_file(repo.root.join(missing)).unwrap();
        let e = Settings::load(&repo.root).unwrap_err();
        assert_eq!(e.exit, Exit::Refused);
        let shown = missing.trim_end_matches("/Cargo.toml");
        assert!(e.message.contains(shown), "{missing}: {}", e.message);
    }
}

#[test]
fn unknown_keys_are_rejected_at_every_level() {
    for (anchor, extra) in [
        ("promotion_wait_seconds = 300", "promotion_wait = 1"),
        ("nextest = \"0.9.151\"", "retries = 0"),
        ("version = \"1.30.0\"", "path = \"x\""),
        ("expect = \"v24.\"", "timeout = 3"),
        ("name = \"npm-ci\"", "when = \"always\""),
        ("compile = true", "parallel = true"),
        ("commit = \"Regenerate API files\"", "amend = true"),
        ("run = \"check\"", "machine = \"mac\""),
    ] {
        let repo = silo_repo();
        let text = SILO.replacen(anchor, &format!("{anchor}\n{extra}"), 1);
        assert_ne!(text, SILO, "{anchor}");
        repo.write(".atelier/atelier.toml", &text);
        let e = Settings::load(&repo.root).unwrap_err();
        assert_eq!(e.exit, Exit::Refused);
        let key = extra.split(' ').next().unwrap();
        assert!(
            e.message.contains(&format!("unknown field `{key}`")),
            "{extra}: {}",
            e.message
        );
    }
}

#[test]
fn semantic_errors_are_all_reported() {
    let repo = silo_repo();
    let text = SILO
        .replace(
            "integration_branch = \"main\"",
            "integration_branch = \"a..b\"",
        )
        .replace("name = \"links\"", "name = \"frontend\"")
        .replace(
            "platforms = [\"macos-aarch64\"]",
            "platforms = [\"linux-aarch64\"]",
        )
        .replace("run = \"check\"", "run = \"full\"");
    repo.write(".atelier/atelier.toml", &text);
    let e = Settings::load(&repo.root).unwrap_err();
    for expect in [
        "integration_branch \"a..b\"",
        "name used twice",
        "platform linux-aarch64 has no [platforms.linux-aarch64] table",
        "\"full\" is neither \"check\" nor a command list",
    ] {
        assert!(e.message.contains(expect), "{expect}: {}", e.message);
    }
}

#[test]
fn rust_exclusions_need_a_reason() {
    let repo = silo_repo();
    let text = SILO.replace(
        "# [[rust.exclude]]",
        "[[rust.exclude]]\nfilter = \"test(/^live_/)\"\nreason = \"\"\n#",
    );
    repo.write(".atelier/atelier.toml", &text);
    let e = Settings::load(&repo.root).unwrap_err();
    assert!(
        e.message.contains("filter and reason are both required"),
        "{}",
        e.message
    );
}
