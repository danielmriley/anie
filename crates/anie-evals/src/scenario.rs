//! Scenario TOML format + deterministic automated checks.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{EvalError, RunMetricsView};

/// One evaluation scenario (single-turn in the first cut).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub family: String,
    #[serde(default)]
    pub description: String,
    /// Optional fixture; omit for a no-setup scenario in a fresh temp dir.
    #[serde(default)]
    pub fixture: Option<Fixture>,
    pub prompt: String,
    pub expect: Expect,
}

/// A fixture is *exactly one* of `dir` or `git_ref` (or omitted).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
}

/// Deterministic, automated pass/fail assertions. No LLM-as-judge.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Final assistant text must contain ALL of these substrings.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Final assistant text must contain AT LEAST ONE of these substrings.
    /// Use this for facts with multiple valid phrasings (e.g. an answer
    /// the model may spell as either of two synonyms) where `contains`'s
    /// all-of semantics would wrongly fail a correct answer.
    #[serde(default)]
    pub contains_any: Vec<String>,
    /// At least one (or `min_tool_calls`) call(s) to this tool.
    #[serde(default)]
    pub must_call_tool: Option<String>,
    #[serde(default)]
    pub min_tool_calls: Option<u32>,
    /// Total tokens must be at or under this cap (efficiency).
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Wall-clock must be at or under this cap.
    #[serde(default)]
    pub max_wall_clock_ms: Option<u64>,
}

impl Expect {
    /// Whether at least one assertion is set. An `[expect]` block with
    /// none would score a vacuous PASS (`all_passed` is true over an
    /// empty result set), so it is rejected at load time.
    #[must_use]
    pub fn has_assertions(&self) -> bool {
        !self.contains.is_empty()
            || !self.contains_any.is_empty()
            || self.must_call_tool.is_some()
            || self.max_tokens.is_some()
            || self.max_wall_clock_ms.is_some()
    }

    /// Whether the scenario carries any content assertion (`contains` or
    /// `contains_any`). The corpus guard requires this for navigation
    /// scenarios so they grade the answer, not just tool choice / budget.
    #[must_use]
    pub fn has_content_assertion(&self) -> bool {
        !self.contains.is_empty() || !self.contains_any.is_empty()
    }
}

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    Fail { reason: String },
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckResult {
    pub check: String,
    #[serde(flatten)]
    pub outcome: CheckOutcome,
}

/// Parse a scenario from TOML text. `deny_unknown_fields` makes a typo'd
/// key fail loudly rather than silently no-op.
pub fn parse_scenario(text: &str) -> Result<Scenario, String> {
    let scenario: Scenario = toml::from_str(text).map_err(|error| error.to_string())?;
    if let Some(fixture) = &scenario.fixture
        && fixture.dir.is_some()
        && fixture.git_ref.is_some()
    {
        return Err("[fixture] must set at most one of `dir` or `git_ref`".to_string());
    }
    if !scenario.expect.has_assertions() {
        return Err(
            "[expect] defines no assertions; a scenario that asserts nothing would always \
             report PASS — add contains / must_call_tool / max_tokens / max_wall_clock_ms"
                .to_string(),
        );
    }
    Ok(scenario)
}

/// Load a scenario from a file path.
pub fn load_scenario(path: &std::path::Path) -> Result<Scenario, EvalError> {
    let text = std::fs::read_to_string(path).map_err(|source| EvalError::ScenarioRead {
        path: path.display().to_string(),
        source,
    })?;
    parse_scenario(&text).map_err(|message| EvalError::ScenarioParse {
        path: path.display().to_string(),
        message,
    })
}

/// Stable content hash of a scenario's raw bytes (reproducibility).
#[must_use]
pub fn scenario_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Evaluate every applicable check against a run's final text + metrics.
#[must_use]
pub fn evaluate_checks(
    expect: &Expect,
    final_text: &str,
    metrics: &RunMetricsView,
) -> Vec<CheckResult> {
    let mut results = Vec::new();

    if !expect.contains.is_empty() {
        let missing: Vec<&String> = expect
            .contains
            .iter()
            .filter(|needle| !final_text.contains(needle.as_str()))
            .collect();
        results.push(CheckResult {
            check: "contains".into(),
            outcome: if missing.is_empty() {
                CheckOutcome::Pass
            } else {
                CheckOutcome::Fail {
                    reason: format!("final text missing: {missing:?}"),
                }
            },
        });
    }

    if !expect.contains_any.is_empty() {
        let found = expect
            .contains_any
            .iter()
            .any(|needle| final_text.contains(needle.as_str()));
        results.push(CheckResult {
            check: "contains_any".into(),
            outcome: if found {
                CheckOutcome::Pass
            } else {
                CheckOutcome::Fail {
                    reason: format!("final text contains none of: {:?}", expect.contains_any),
                }
            },
        });
    }

    if let Some(tool) = &expect.must_call_tool {
        let min = expect.min_tool_calls.unwrap_or(1);
        let calls = metrics
            .tools
            .by_tool
            .get(tool)
            .map_or(0, |outcome| outcome.calls);
        results.push(CheckResult {
            check: "must_call_tool".into(),
            outcome: if calls >= min {
                CheckOutcome::Pass
            } else {
                CheckOutcome::Fail {
                    reason: format!("tool `{tool}` called {calls} times, need >= {min}"),
                }
            },
        });
    }

    if let Some(cap) = expect.max_tokens {
        let total = metrics.tokens.total_tokens;
        results.push(CheckResult {
            check: "max_tokens".into(),
            outcome: if total <= cap {
                CheckOutcome::Pass
            } else {
                CheckOutcome::Fail {
                    reason: format!("{total} tokens > cap {cap}"),
                }
            },
        });
    }

    if let Some(cap) = expect.max_wall_clock_ms {
        let elapsed = metrics.wall_clock_ms;
        results.push(CheckResult {
            check: "max_wall_clock_ms".into(),
            outcome: if elapsed <= cap {
                CheckOutcome::Pass
            } else {
                CheckOutcome::Fail {
                    reason: format!("{elapsed}ms > cap {cap}ms"),
                }
            },
        });
    }

    results
}

/// Whether every check passed (a NotApplicable check does not fail).
#[must_use]
pub fn all_passed(results: &[CheckResult]) -> bool {
    results
        .iter()
        .all(|result| !matches!(result.outcome, CheckOutcome::Fail { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TokenView, ToolOutcomeView, ToolView};

    fn metrics(total_tokens: u64, wall_clock_ms: u64) -> RunMetricsView {
        RunMetricsView {
            schema_version: crate::EXPECTED_RUN_METRICS_SCHEMA_VERSION,
            harness_mode: "current".into(),
            wall_clock_ms,
            turns: 1,
            tokens: TokenView { total_tokens },
            tools: ToolView::default(),
            tool_repair: crate::ToolRepairView::default(),
            recovery: crate::RecoveryView::default(),
            prompt: crate::PromptView::default(),
            context: crate::ContextView::default(),
            edit_guard: crate::EditGuardView::default(),
        }
    }

    #[test]
    fn loads_minimal_scenario_with_no_fixture() {
        let s = parse_scenario(
            "name = \"x\"\nfamily = \"f\"\nprompt = \"do it\"\n[expect]\ncontains = [\"y\"]\n",
        )
        .expect("parse");
        assert_eq!(s.name, "x");
        assert!(s.fixture.is_none());
        assert_eq!(s.expect.contains, vec!["y".to_string()]);
    }

    #[test]
    fn rejects_expect_with_no_assertions() {
        let err = parse_scenario("name = \"x\"\nfamily = \"f\"\nprompt = \"p\"\n[expect]\n")
            .expect_err("assertion-free expect must be rejected");
        assert!(err.contains("no assertions"), "{err}");
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let err =
            parse_scenario("name = \"x\"\nfamily = \"f\"\nprompt = \"p\"\nbogus = 1\n[expect]\n")
                .expect_err("unknown key");
        assert!(err.contains("bogus") || err.contains("unknown"), "{err}");
    }

    #[test]
    fn rejects_fixture_with_both_dir_and_git_ref() {
        let err = parse_scenario(
            "name = \"x\"\nfamily = \"f\"\nprompt = \"p\"\n[fixture]\ndir = \"a\"\ngit_ref = \"b\"\n[expect]\n",
        )
        .expect_err("both fixture sources");
        assert!(err.contains("at most one"), "{err}");
    }

    #[test]
    fn contains_check_fails_when_any_required_string_absent() {
        let expect = Expect {
            contains: vec!["alpha".into(), "missing".into()],
            ..Expect::default()
        };
        let results = evaluate_checks(&expect, "alpha only", &metrics(0, 0));
        assert!(!all_passed(&results));
        assert!(matches!(results[0].outcome, CheckOutcome::Fail { .. }));
    }

    #[test]
    fn contains_any_passes_when_at_least_one_alternative_present() {
        let expect = Expect {
            contains_any: vec!["alpha".into(), "beta".into()],
            ..Expect::default()
        };
        // Only one of the two alternatives is present — that is enough.
        let results = evaluate_checks(&expect, "only beta here", &metrics(0, 0));
        assert!(all_passed(&results));
        assert_eq!(results[0].check, "contains_any");
    }

    #[test]
    fn contains_any_fails_only_when_no_alternative_present() {
        let expect = Expect {
            contains_any: vec!["alpha".into(), "beta".into()],
            ..Expect::default()
        };
        let results = evaluate_checks(&expect, "neither word", &metrics(0, 0));
        assert!(!all_passed(&results));
        assert!(matches!(results[0].outcome, CheckOutcome::Fail { .. }));
    }

    #[test]
    fn contains_any_round_trips_through_toml() {
        let s = parse_scenario(
            "name = \"x\"\nfamily = \"f\"\nprompt = \"p\"\n[expect]\ncontains_any = [\"a\", \"b\"]\n",
        )
        .expect("parse");
        assert_eq!(
            s.expect.contains_any,
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(s.expect.has_assertions());
        assert!(s.expect.has_content_assertion());
    }

    #[test]
    fn contains_any_alone_satisfies_the_assertion_guard() {
        // A scenario whose only assertion is contains_any must not be
        // rejected as assertion-free.
        parse_scenario(
            "name = \"x\"\nfamily = \"f\"\nprompt = \"p\"\n[expect]\ncontains_any = [\"a\"]\n",
        )
        .expect("contains_any counts as an assertion");
    }

    #[test]
    fn must_call_tool_check_reads_tool_metrics_by_name() {
        let expect = Expect {
            must_call_tool: Some("grep".into()),
            min_tool_calls: Some(2),
            ..Expect::default()
        };
        let mut m = metrics(0, 0);
        m.tools.by_tool.insert(
            "grep".into(),
            ToolOutcomeView {
                calls: 2,
                failures: 0,
            },
        );
        assert!(all_passed(&evaluate_checks(&expect, "", &m)));
        // One call is below the min of 2.
        m.tools.by_tool.insert(
            "grep".into(),
            ToolOutcomeView {
                calls: 1,
                failures: 0,
            },
        );
        assert!(!all_passed(&evaluate_checks(&expect, "", &m)));
    }

    #[test]
    fn max_tokens_check_fails_above_cap_passes_at_cap() {
        let expect = Expect {
            max_tokens: Some(100),
            ..Expect::default()
        };
        assert!(all_passed(&evaluate_checks(&expect, "", &metrics(100, 0))));
        assert!(!all_passed(&evaluate_checks(&expect, "", &metrics(101, 0))));
    }

    #[test]
    fn scenario_sha256_is_stable_for_identical_bytes() {
        let bytes = b"name = \"x\"\n";
        assert_eq!(scenario_sha256(bytes), scenario_sha256(bytes));
        assert_ne!(scenario_sha256(bytes), scenario_sha256(b"different"));
    }
}
