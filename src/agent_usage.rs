//! Prompt-usage accounting per agent seat.
//!
//! Usage is reported by ACP sessions as cumulative session totals, so every
//! record is folded into a per-(seat, model, session) baseline and only the
//! delta is accumulated. Totals are kept per seat and, additionally, per model
//! so the UI can show where the tokens went.

use std::collections::{BTreeMap, HashMap};

use agent_client_protocol::schema::v1::{Usage, UsageUpdate};
use serde::{Deserialize, Serialize};

/// Which agent produced a prompt. Subagent work is one seat regardless of what
/// the subagent was asked to do; discrete-review lanes and their supervisor
/// bill to `Review` so review overhead stays visible on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Seat {
    Primary,
    Subagent,
    Review,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub seat: Seat,
    /// The model that served this prompt, when known.
    pub model: Option<String>,
    pub usage: Option<Usage>,
    pub update: Option<UsageUpdate>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoleUsage {
    pub prompts: u64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thought_tokens: u64,
    pub context_used: u64,
    pub context_size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub costs: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub primary: RoleUsage,
    pub subagents: RoleUsage,
    pub review: RoleUsage,
    /// Per-model totals across every seat, in model-name order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_model: BTreeMap<String, RoleUsage>,
    #[serde(skip)]
    baselines: HashMap<(Seat, Option<String>, Option<String>), Baseline>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Baseline {
    session_id: String,
    total_tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    thought_tokens: u64,
    costs: BTreeMap<String, f64>,
}

fn counter_delta(current: u64, previous: u64) -> u64 {
    current.checked_sub(previous).unwrap_or(current)
}

fn cost_delta(current: f64, previous: f64) -> f64 {
    if current >= previous {
        current - previous
    } else {
        current
    }
}

/// The deltas one record contributes, computed once against its baseline and
/// then applied to every bucket the record belongs to.
#[derive(Debug, Default)]
struct Delta {
    total_tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    thought_tokens: u64,
    context_used: Option<u64>,
    context_size: Option<u64>,
    costs: BTreeMap<String, f64>,
}

impl Delta {
    fn apply(&self, usage: &mut RoleUsage) {
        usage.prompts += 1;
        usage.total_tokens += self.total_tokens;
        usage.input_tokens += self.input_tokens;
        usage.output_tokens += self.output_tokens;
        usage.thought_tokens += self.thought_tokens;
        if let Some(used) = self.context_used {
            usage.context_used = used;
        }
        if let Some(size) = self.context_size {
            usage.context_size = size;
        }
        for (currency, amount) in &self.costs {
            *usage.costs.entry(currency.clone()).or_default() += amount;
        }
    }
}

impl Snapshot {
    pub fn observe(&mut self, record: Record) {
        // A baseline is per seat, per model, per ACP session: a new session on
        // the same model restarts its cumulative counters from zero, and two
        // concurrent subagents on one model never share a baseline.
        let lane = (record.seat, record.model.clone(), record.session_id.clone());
        let same_session = record.session_id.as_ref().is_some_and(|session_id| {
            self.baselines
                .get(&lane)
                .is_some_and(|baseline| baseline.session_id == *session_id)
        });
        let previous = same_session
            .then(|| self.baselines.get(&lane).cloned())
            .flatten()
            .unwrap_or_default();
        let mut next = record.session_id.as_ref().map(|session_id| {
            if same_session {
                let mut next = previous.clone();
                next.session_id = session_id.clone();
                next
            } else {
                Baseline {
                    session_id: session_id.clone(),
                    ..Baseline::default()
                }
            }
        });

        let mut delta = Delta::default();
        if let Some(value) = record.usage {
            delta.total_tokens = counter_delta(value.total_tokens, previous.total_tokens);
            delta.input_tokens = counter_delta(value.input_tokens, previous.input_tokens);
            delta.output_tokens = counter_delta(value.output_tokens, previous.output_tokens);
            delta.thought_tokens = counter_delta(
                value.thought_tokens.unwrap_or_default(),
                previous.thought_tokens,
            );
            if let Some(next) = next.as_mut() {
                next.total_tokens = value.total_tokens;
                next.input_tokens = value.input_tokens;
                next.output_tokens = value.output_tokens;
                next.thought_tokens = value.thought_tokens.unwrap_or_default();
            }
        }
        if let Some(update) = record.update {
            delta.context_used = Some(update.used);
            delta.context_size = Some(update.size);
            if let Some(cost) = update.cost {
                let previous_cost = previous
                    .costs
                    .get(&cost.currency)
                    .copied()
                    .unwrap_or_default();
                delta.costs.insert(
                    cost.currency.clone(),
                    cost_delta(cost.amount, previous_cost),
                );
                if let Some(next) = next.as_mut() {
                    next.costs.insert(cost.currency, cost.amount);
                }
            }
        }

        delta.apply(match record.seat {
            Seat::Primary => &mut self.primary,
            Seat::Subagent => &mut self.subagents,
            Seat::Review => &mut self.review,
        });
        if let Some(model) = record.model {
            delta.apply(self.per_model.entry(model).or_default());
        }
        if let Some(next) = next {
            self.baselines.insert(lane, next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seats_are_separate_and_aggregate() {
        let mut snapshot = Snapshot::default();
        for (seat, total) in [
            (Seat::Primary, 10),
            (Seat::Subagent, 20),
            (Seat::Review, 30),
        ] {
            snapshot.observe(Record {
                seat,
                model: None,
                usage: Some(Usage::new(total, total - 3, 3)),
                update: None,
                session_id: None,
            });
        }

        assert_eq!(snapshot.primary.total_tokens, 10);
        assert_eq!(snapshot.subagents.total_tokens, 20);
        assert_eq!(snapshot.review.total_tokens, 30);
    }

    #[test]
    fn per_model_totals_span_seats() {
        let mut snapshot = Snapshot::default();
        snapshot.observe(Record {
            seat: Seat::Subagent,
            model: Some("gpt-x".into()),
            usage: Some(Usage::new(10, 7, 3)),
            update: None,
            session_id: Some("a".into()),
        });
        snapshot.observe(Record {
            seat: Seat::Review,
            model: Some("gpt-x".into()),
            usage: Some(Usage::new(5, 4, 1)),
            update: None,
            session_id: Some("b".into()),
        });
        snapshot.observe(Record {
            seat: Seat::Primary,
            model: Some("claude-y".into()),
            usage: Some(Usage::new(7, 6, 1)),
            update: None,
            session_id: Some("c".into()),
        });

        assert_eq!(snapshot.per_model["gpt-x"].total_tokens, 15);
        assert_eq!(snapshot.per_model["gpt-x"].prompts, 2);
        assert_eq!(snapshot.per_model["claude-y"].total_tokens, 7);
    }

    #[test]
    fn cumulative_session_usage_is_added_as_deltas() {
        let mut snapshot = Snapshot::default();
        for total in [100, 140, 140] {
            snapshot.observe(Record {
                seat: Seat::Primary,
                model: Some("gpt-x".into()),
                usage: Some(Usage::new(total, total, 0)),
                update: None,
                session_id: Some("primary-1".into()),
            });
        }
        assert_eq!(snapshot.primary.total_tokens, 140);
        assert_eq!(snapshot.per_model["gpt-x"].total_tokens, 140);
    }

    #[test]
    fn a_new_session_establishes_a_new_usage_baseline() {
        let mut snapshot = Snapshot::default();
        for (session_id, total) in [("one", 100), ("two", 25)] {
            snapshot.observe(Record {
                seat: Seat::Primary,
                model: None,
                usage: Some(Usage::new(total, total, 0)),
                update: None,
                session_id: Some(session_id.into()),
            });
        }
        assert_eq!(snapshot.primary.total_tokens, 125);
    }

    #[test]
    fn concurrent_subagent_sessions_keep_independent_baselines() {
        let mut snapshot = Snapshot::default();
        // Two subagents on the same model interleave cumulative reports; each
        // must be measured against its own session baseline.
        for (session_id, total) in [("a", 100), ("b", 40), ("a", 130), ("b", 60)] {
            snapshot.observe(Record {
                seat: Seat::Subagent,
                model: Some("gpt-x".into()),
                usage: Some(Usage::new(total, total, 0)),
                update: None,
                session_id: Some(session_id.into()),
            });
        }
        assert_eq!(snapshot.subagents.total_tokens, 190);
    }
}
