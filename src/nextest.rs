//! cargo-nextest as `check` uses it.
//!
//! - The flags of the test step, and the filterset that leaves out `rust.exclude` entries.
//! - The refusals for a workspace's `.config/nextest.toml`: a `default-filter` leaves tests out
//!   without saying so, a retry or `flaky-result = "pass"` lets a failing test count as passed.
//!   Project steps call nextest themselves, so atelier cannot add flags there and refuses the file.
//! - Reading nextest's machine-readable output (libtest JSON, an experimental format of the pinned
//!   version) into per-test results.

use serde_json::Value;

use crate::settings::Exclude;

/// The flags of atelier's own nextest run, in order, after `cargo nextest run`.
pub const FLAGS: &[&str] = &[
    "--workspace",
    "--locked",
    "--no-fail-fast",
    "--retries",
    "0",
    "--flaky-result",
    "fail",
    "--ignore-default-filter",
];

/// Environment of every job atelier starts: nextest then ignores a value exported in a shell and
/// any file setting, as the flags do.
pub const JOB_ENV: &[(&str, &str)] = &[("NEXTEST_RETRIES", "0"), ("NEXTEST_FLAKY_RESULT", "fail")];

/// Environment that makes the test step print libtest JSON on standard output.
pub const JSON_ENV: &[(&str, &str)] = &[
    ("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1"),
    ("NEXTEST_MESSAGE_FORMAT", "libtest-json"),
];

/// `not ((<filter 1>) | (<filter 2>))`, or `None` without exclusions.
pub fn exclusion_expression(exclude: &[Exclude]) -> Option<String> {
    if exclude.is_empty() {
        return None;
    }
    let any = exclude
        .iter()
        .map(|e| format!("({})", e.filter))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(format!("not ({any})"))
}

/// The arguments after `cargo` for the test step.
pub fn run_args(exclude: &[Exclude]) -> Vec<String> {
    let mut args: Vec<String> = ["nextest", "run"]
        .into_iter()
        .chain(FLAGS.iter().copied())
        .map(str::to_owned)
        .collect();
    if let Some(expr) = exclusion_expression(exclude) {
        args.push("-E".to_owned());
        args.push(expr);
    }
    args
}

/// What is wrong with a nextest configuration file, one line per problem (empty: nothing).
/// `file` names the file in the messages.
pub fn config_problems(file: &str, text: &str) -> Vec<String> {
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            let first = msg.lines().next().unwrap_or("").trim();
            return vec![format!("{file}: not valid TOML ({first})")];
        }
    };
    let mut problems = Vec::new();
    if table
        .get("experimental")
        .is_some_and(|v| mentions(v, "wrapper-scripts"))
    {
        problems.push(format!(
            "{file}: experimental wrapper-scripts runs every test binary under a command from the file, which can report a failing test as passed"
        ));
    }
    if table
        .get("scripts")
        .and_then(|s| s.get("wrapper"))
        .and_then(toml::Value::as_table)
        .is_some_and(|t| !t.is_empty())
    {
        problems.push(format!(
            "{file}: scripts.wrapper defines a wrapper around the test binaries, which can report a failing test as passed"
        ));
    }
    let Some(profiles) = table.get("profile").and_then(toml::Value::as_table) else {
        return problems;
    };
    for (name, profile) in profiles {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        let at = format!("{file}: profile.{name}");
        table_problems(&at, profile, &mut problems);
        if let Some(scripts) = profile.get("scripts").and_then(toml::Value::as_array) {
            for (i, s) in scripts.iter().enumerate() {
                if let Some(wrapper) = s.as_table().and_then(|s| s.get("run-wrapper")) {
                    problems.push(format!(
                        "{at}.scripts[{i}].run-wrapper is {wrapper}: the wrapper can report a failing test as passed"
                    ));
                }
            }
        }
        if let Some(overrides) = profile.get("overrides").and_then(toml::Value::as_array) {
            for (i, o) in overrides.iter().enumerate() {
                if let Some(o) = o.as_table() {
                    table_problems(&format!("{at}.overrides[{i}]"), o, &mut problems);
                }
            }
        }
    }
    problems
}

/// Whether a value names `word`: a string, a list holding it, or a table keyed by it.
fn mentions(v: &toml::Value, word: &str) -> bool {
    match v {
        toml::Value::String(s) => s == word,
        toml::Value::Array(a) => a.iter().any(|x| mentions(x, word)),
        toml::Value::Table(t) => t.contains_key(word),
        _ => false,
    }
}

/// The settings that can hide a test, in a profile or in one of its overrides.
fn table_problems(at: &str, table: &toml::Table, problems: &mut Vec<String>) {
    if table.contains_key("default-filter") {
        problems.push(format!(
            "{at}.default-filter leaves tests out without saying so"
        ));
    }
    if let Some(v) = table
        .get("slow-timeout")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("on-timeout"))
        && v.as_str() != Some("fail")
    {
        problems.push(format!(
            "{at}.slow-timeout sets on-timeout = {v}: a test that hangs would count as passed"
        ));
    }
    if let Some(retries) = table.get("retries") {
        let count = match retries {
            toml::Value::Integer(n) => Some(*n),
            toml::Value::Table(t) => t.get("count").and_then(toml::Value::as_integer),
            _ => None,
        };
        match count {
            Some(0) => {}
            Some(n) => problems.push(format!("{at}.retries is {n}: a retry hides a failing test")),
            None => problems.push(format!(
                "{at}.retries is not a retry count atelier can read"
            )),
        }
    }
    if let Some(v) = table.get("flaky-result")
        && v.as_str() != Some("fail")
    {
        problems.push(format!(
            "{at}.flaky-result is {v}: a test that passes only on a retry would count as passed"
        ));
    }
}

/// How one test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    Ignored,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Passed => "pass",
            Verdict::Failed => "fail",
            Verdict::Ignored => "ignored",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestResult {
    pub name: String,
    pub verdict: Verdict,
    pub seconds: Option<f64>,
}

/// The tests that ended in a nextest libtest-JSON stream; lines that are not test results
/// (started events, suite summaries, anything unreadable) are skipped.
pub fn parse_results(stream: &str) -> Vec<TestResult> {
    let mut out = Vec::new();
    for line in stream.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("test") {
            continue;
        }
        let verdict = match v.get("event").and_then(Value::as_str) {
            Some("ok") => Verdict::Passed,
            Some("failed") | Some("timeout") => Verdict::Failed,
            Some("ignored") => Verdict::Ignored,
            _ => continue,
        };
        let Some(name) = v.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(TestResult {
            name: name.to_owned(),
            verdict,
            seconds: v.get("exec_time").and_then(Value::as_f64),
        });
    }
    out
}

/// The number of tests a `cargo nextest list --message-format json` output says match its
/// filterset (ignored tests do not).
pub fn count_matching(list_json: &str) -> usize {
    let Ok(v) = serde_json::from_str::<Value>(list_json) else {
        return 0;
    };
    let Some(suites) = v.get("rust-suites").and_then(Value::as_object) else {
        return 0;
    };
    suites
        .values()
        .filter_map(|s| s.get("testcases").and_then(Value::as_object))
        .flat_map(|cases| cases.values())
        .filter(|c| {
            c.get("ignored").and_then(Value::as_bool) != Some(true)
                && c.pointer("/filter-match/status").and_then(Value::as_str) == Some("matches")
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(filter: &str, reason: &str) -> Exclude {
        Exclude {
            filter: filter.to_owned(),
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn the_flags_are_the_design_flags() {
        let args = run_args(&[]);
        assert_eq!(
            args.join(" "),
            "nextest run --workspace --locked --no-fail-fast --retries 0 --flaky-result fail --ignore-default-filter"
        );
    }

    #[test]
    fn exclusions_become_one_expression() {
        assert_eq!(exclusion_expression(&[]), None);
        assert_eq!(
            exclusion_expression(&[ex("test(/^live_/)", "live")]).unwrap(),
            "not ((test(/^live_/)))"
        );
        let args = run_args(&[ex("test(a)", "x"), ex("package(b)", "y")]);
        assert_eq!(args[args.len() - 2], "-E");
        assert_eq!(args[args.len() - 1], "not ((test(a)) | (package(b)))");
    }

    #[test]
    fn refused_nextest_files() {
        let p = |t: &str| config_problems(".config/nextest.toml", t);
        assert!(p("").is_empty());
        assert!(p("[profile.default]\nfail-fast = false\nretries = 0\n").is_empty());
        assert!(p("[profile.ci]\nretries = { backoff = \"fixed\", count = 0 }\n").is_empty());
        assert!(p("[profile.default]\nflaky-result = \"fail\"\n").is_empty());

        let got = p("[profile.default]\nretries = 1\n");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("profile.default.retries is 1"));
        assert!(!p("[profile.ci]\nretries = { backoff = \"fixed\", count = 2 }\n").is_empty());
        assert!(!p("[profile.default]\ndefault-filter = \"not test(slow)\"\n").is_empty());
        assert!(!p("[profile.default]\nflaky-result = \"pass\"\n").is_empty());
        let got = p("[[profile.default.overrides]]\nfilter = \"test(x)\"\nretries = 3\n");
        assert!(got[0].contains("overrides[0].retries"), "{got:?}");
        assert!(!p("[profile.default\n").is_empty());
    }

    #[test]
    fn nextest_settings_that_count_a_failing_test_as_passed_are_refused() {
        let p = |t: &str| config_problems(".config/nextest.toml", t);
        let got = p(
            "[profile.default]\nslow-timeout = { period = \"1s\", terminate-after = 2, on-timeout = \"pass\" }\n",
        );
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("profile.default.slow-timeout sets on-timeout"));
        let got = p(
            "[[profile.ci.overrides]]\nfilter = \"all()\"\nslow-timeout = { period = \"1s\", on-timeout = \"pass\" }\n",
        );
        assert!(
            got[0].contains("profile.ci.overrides[0].slow-timeout"),
            "{got:?}"
        );
        assert!(!p("[profile.default.slow-timeout]\nperiod = \"1s\"\non-timeout = 3\n").is_empty());
        assert!(
            p("[profile.default]\nslow-timeout = { period = \"1s\", on-timeout = \"fail\" }\n")
                .is_empty()
        );
        assert!(p("[profile.default]\nslow-timeout = \"60s\"\n").is_empty());

        let got = p(
            "[[profile.default.overrides]]\nplatform = \"cfg(all())\"\ndefault-filter = \"not test(x)\"\n",
        );
        assert!(got[0].contains("overrides[0].default-filter"), "{got:?}");

        let wrapper = "experimental = [\"wrapper-scripts\"]\n[scripts.wrapper.w]\ncommand = \"sh\"\n[[profile.default.scripts]]\nfilter = \"all()\"\nrun-wrapper = \"w\"\n";
        let got = p(wrapper);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(
            got.iter()
                .any(|g| g.contains("experimental wrapper-scripts"))
        );
        assert!(got.iter().any(|g| g.contains("scripts.wrapper defines")));
        assert!(got.iter().any(|g| g.contains("scripts[0].run-wrapper")));
        // Setup scripts do not decide a verdict.
        assert!(p("experimental = [\"setup-scripts\"]\n[scripts.setup.s]\ncommand = \"true\"\n[[profile.default.scripts]]\nfilter = \"all()\"\nsetup = \"s\"\n").is_empty());
    }

    #[test]
    fn counts_the_tests_a_filterset_matches() {
        let json = r#"{"rust-suites":{"a":{"testcases":{
            "one":{"ignored":false,"filter-match":{"status":"matches"}},
            "two":{"ignored":false,"filter-match":{"status":"mismatch","reason":"expression"}},
            "three":{"ignored":true,"filter-match":{"status":"mismatch","reason":"ignored"}}}}}}"#;
        assert_eq!(count_matching(json), 1);
        assert_eq!(count_matching("garbage"), 0);
    }

    #[test]
    fn libtest_json_results() {
        let stream = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":3}\n",
            "{\"type\":\"test\",\"event\":\"started\",\"name\":\"a::a$t::one\"}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"a::a$t::one\",\"exec_time\":0.5}\n",
            "{\"type\":\"test\",\"event\":\"failed\",\"name\":\"a::a$t::two\",\"stdout\":\"x\"}\n",
            "{\"type\":\"test\",\"event\":\"ignored\",\"name\":\"a::a$t::three\"}\n",
            "not json\n",
        );
        let r = parse_results(stream);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].verdict, Verdict::Passed);
        assert_eq!(r[0].seconds, Some(0.5));
        assert_eq!(r[1].verdict, Verdict::Failed);
        assert_eq!(r[2].verdict, Verdict::Ignored);
    }
}
