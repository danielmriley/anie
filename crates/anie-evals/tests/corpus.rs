//! Guard: every committed scenario file parses and is well-formed, so
//! the corpus can't drift into invalid TOML.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use anie_evals::load_scenario;

#[test]
fn every_committed_scenario_parses_and_is_well_formed() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut count = 0;
    let mut families = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(&dir).expect("scenarios dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let scenario = load_scenario(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!scenario.name.trim().is_empty(), "{}", path.display());
        assert!(!scenario.prompt.trim().is_empty(), "{}", path.display());
        // Each scenario must assert *something*.
        let e = &scenario.expect;
        assert!(
            !e.contains.is_empty()
                || !e.contains_any.is_empty()
                || e.must_call_tool.is_some()
                || e.max_tokens.is_some()
                || e.max_wall_clock_ms.is_some(),
            "{} has no checks",
            path.display()
        );
        // Navigation scenarios grade the *answer*, not tool choice. After
        // dropping incidental `must_call_tool` checks (bench/PR1), a nav
        // scenario with only a budget cap would score correct answers as
        // failures, so every one must carry a content assertion.
        if scenario.family == "repo_navigation" {
            assert!(
                e.has_content_assertion(),
                "{} is a repo_navigation scenario but has no contains/contains_any assertion",
                path.display()
            );
        }
        families.insert(scenario.family.clone());
        count += 1;
    }
    assert!(count >= 4, "expected >= 4 scenarios, found {count}");
    assert!(
        families.len() >= 2,
        "expected >= 2 families, found {families:?}"
    );
}
