//! Budget-enforcement policy.
//!
//! A [`BeforeModelPolicy`] that halts a run at the next step boundary
//! when a configured cost/token ceiling is reached. Pure comparison of
//! the run's accumulated usage (plus the session baseline captured at
//! run start) against the ceilings; returns
//! `BeforeModelResponse::StopRun(RunStopReason::BudgetExceeded { .. })`
//! — a typed terminal condition, never a panic or a `ProviderError`.
//! Opt-in: installed only when at least one ceiling is configured, so an
//! unset `[budget]` is byte-identical to today. See `docs/cost_budget/`.

use async_trait::async_trait;

use anie_agent::{
    BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse, BudgetLimit, BudgetScope,
    RunStopReason,
};
use anie_config::BudgetConfig;
use anie_protocol::Usage;
use anie_provider::CostPerMillion;

pub(crate) struct BudgetPolicy {
    config: BudgetConfig,
    pricing: CostPerMillion,
    /// Session usage already spent before this run started; run usage is
    /// added on top to compare against session ceilings.
    session_baseline: Usage,
}

fn tokens_of(usage: &Usage) -> u64 {
    usage.input_tokens + usage.output_tokens + usage.cache_read_tokens + usage.cache_write_tokens
}

impl BudgetPolicy {
    pub(crate) fn new(
        config: BudgetConfig,
        pricing: CostPerMillion,
        session_baseline: Usage,
    ) -> Self {
        Self {
            config,
            pricing,
            session_baseline,
        }
    }

    /// Whether any ceiling is configured. The controller only installs
    /// the policy when this is true, keeping the unset path zero-cost.
    pub(crate) fn is_enforcing(&self) -> bool {
        self.config.max_run_cost_usd.is_some()
            || self.config.max_session_cost_usd.is_some()
            || self.config.max_run_tokens.is_some()
            || self.config.max_session_tokens.is_some()
    }

    /// First ceiling tripped by `run_usage`, if any. Checked in a stable
    /// order: run cost, run tokens, session cost, session tokens.
    fn check(&self, run_usage: &Usage) -> Option<RunStopReason> {
        let run_cost = self.pricing.cost_of(run_usage).total;
        let run_tokens = tokens_of(run_usage);
        let session_cost = self.pricing.cost_of(&self.session_baseline).total + run_cost;
        let session_tokens = tokens_of(&self.session_baseline) + run_tokens;

        if let Some(limit) = self.config.max_run_cost_usd
            && run_cost >= limit
        {
            return Some(exceeded(
                BudgetScope::Run,
                BudgetLimit::Cost(limit),
                run_cost,
            ));
        }
        if let Some(limit) = self.config.max_run_tokens
            && run_tokens >= limit
        {
            return Some(exceeded(
                BudgetScope::Run,
                BudgetLimit::Tokens(limit),
                run_tokens as f64,
            ));
        }
        if let Some(limit) = self.config.max_session_cost_usd
            && session_cost >= limit
        {
            return Some(exceeded(
                BudgetScope::Session,
                BudgetLimit::Cost(limit),
                session_cost,
            ));
        }
        if let Some(limit) = self.config.max_session_tokens
            && session_tokens >= limit
        {
            return Some(exceeded(
                BudgetScope::Session,
                BudgetLimit::Tokens(limit),
                session_tokens as f64,
            ));
        }
        None
    }
}

fn exceeded(scope: BudgetScope, limit: BudgetLimit, spent: f64) -> RunStopReason {
    RunStopReason::BudgetExceeded {
        scope,
        limit,
        spent,
    }
}

#[async_trait]
impl BeforeModelPolicy for BudgetPolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        match self.check(request.run_usage) {
            Some(reason) => BeforeModelResponse::StopRun(reason),
            None => BeforeModelResponse::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates() -> CostPerMillion {
        CostPerMillion {
            input: 3.0,
            output: 15.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    async fn run(policy: &BudgetPolicy, run_usage: &Usage) -> BeforeModelResponse {
        // BeforeModelRequest borrows; build the minimal shape.
        let model = anie_provider::Model {
            id: "m".into(),
            name: "m".into(),
            provider: "p".into(),
            api: anie_provider::ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 1000,
            max_tokens: 100,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: anie_provider::ModelCompat::None,
        };
        policy
            .before_model(BeforeModelRequest {
                context: &[],
                generated_messages: &[],
                model: &model,
                step_index: 0,
                run_usage,
            })
            .await
    }

    #[tokio::test]
    async fn budget_policy_continues_when_no_ceiling_configured() {
        let policy = BudgetPolicy::new(BudgetConfig::default(), rates(), Usage::default());
        assert!(!policy.is_enforcing());
        assert_eq!(
            run(&policy, &usage(10_000_000, 0)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn budget_policy_stops_run_when_run_cost_exceeds_ceiling() {
        let config = BudgetConfig {
            max_run_cost_usd: Some(1.0),
            ..BudgetConfig::default()
        };
        let policy = BudgetPolicy::new(config, rates(), Usage::default());
        // 1M input * $3 = $3 >= $1 ceiling.
        match run(&policy, &usage(1_000_000, 0)).await {
            BeforeModelResponse::StopRun(RunStopReason::BudgetExceeded {
                scope: BudgetScope::Run,
                limit: BudgetLimit::Cost(_),
                ..
            }) => {}
            other => panic!("expected run-cost stop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn budget_policy_stops_run_when_run_token_ceiling_exceeded() {
        let config = BudgetConfig {
            max_run_tokens: Some(1_000),
            ..BudgetConfig::default()
        };
        let policy = BudgetPolicy::new(config, CostPerMillion::zero(), Usage::default());
        match run(&policy, &usage(600, 600)).await {
            BeforeModelResponse::StopRun(RunStopReason::BudgetExceeded {
                scope: BudgetScope::Run,
                limit: BudgetLimit::Tokens(1_000),
                ..
            }) => {}
            other => panic!("expected run-token stop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn budget_policy_counts_session_baseline_plus_run_usage_against_session_ceiling() {
        let config = BudgetConfig {
            max_session_cost_usd: Some(10.0),
            ..BudgetConfig::default()
        };
        // Baseline already spent $9 (3M input * $3); this run adds $3 more
        // → $12 >= $10 session ceiling, even though the run alone is under.
        let policy = BudgetPolicy::new(config, rates(), usage(3_000_000, 0));
        match run(&policy, &usage(1_000_000, 0)).await {
            BeforeModelResponse::StopRun(RunStopReason::BudgetExceeded {
                scope: BudgetScope::Session,
                ..
            }) => {}
            other => panic!("expected session stop, got {other:?}"),
        }
        // A tiny run that doesn't cross the session ceiling continues.
        let policy = BudgetPolicy::new(
            BudgetConfig {
                max_session_cost_usd: Some(10.0),
                ..BudgetConfig::default()
            },
            rates(),
            usage(1_000_000, 0),
        );
        assert_eq!(
            run(&policy, &usage(100_000, 0)).await,
            BeforeModelResponse::Continue
        );
    }
}
