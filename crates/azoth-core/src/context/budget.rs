//! Per-lane token budgeting for the composite evidence collector.
//!
//! Risk ledger #4 — "Token budget starvation under pathological
//! weights": a greedy-by-weight cut after reranking can let one lane
//! crowd out the others when the weight distribution is skewed. The
//! `TokenBudget` split into a global ceiling plus a per-lane floor
//! solves this without introducing a strict per-lane cap.
//!
//! ## Algorithm
//!
//! `apply()` takes a list of `(lane, token_cost)` slots sorted by
//! reranker score (descending) and returns the indices to keep.
//!
//! 1. Reserve the floor for each lane by walking the list *in order*
//!    and admitting a slot whenever its lane has not yet met its
//!    floor — regardless of whether the global budget is exhausted.
//!    This guarantees no lane starves.
//! 2. Walk the remainder (non-reserved slots) in the same order and
//!    admit whatever fits in what is left of the global budget.
//!
//! The ordering is stable: we only partition into admitted / rejected,
//! never reshuffle. Downstream `compile()` still sorts by
//! `decision_weight` descending, so composite output remains
//! "critical-first" within the kept set.

use std::collections::HashMap;

/// Per-lane-floor + global-ceiling budget.
///
/// Defaults match the v2 plan risk ledger #4:
/// `graph=200 · symbol=400 · lexical=400 · fts=400 · checkpoint=200`.
/// Defaults keep the pre-v2 behaviour on unknown lanes (`None` floor →
/// treated as 0, same as an absent entry).
#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub max_tokens: u32,
    pub per_lane_floor: HashMap<&'static str, u32>,
}

impl TokenBudget {
    /// Ship-default budget per v2 plan risk ledger #4. 4096 tokens is
    /// the total; floors sum to 1600, leaving 2496 tokens for the
    /// greedy fill.
    pub fn v2_default() -> Self {
        let mut per_lane_floor = HashMap::new();
        per_lane_floor.insert("graph", 200);
        per_lane_floor.insert("symbol", 400);
        per_lane_floor.insert("lexical", 400);
        per_lane_floor.insert("fts", 400);
        per_lane_floor.insert("checkpoint", 200);
        Self {
            max_tokens: 4096,
            per_lane_floor,
        }
    }

    /// Admit a subset of `slots` (each tagged with its lane and token
    /// cost) honouring the per-lane floors first and the global
    /// ceiling second. Returns the indices into `slots` of admitted
    /// items, in original order.
    ///
    /// The input is expected to be sorted by reranker score
    /// descending — the first slot in each lane is the most relevant
    /// one, so the floor is claimed by the best items per lane.
    pub fn apply(&self, slots: &[Slot]) -> Vec<usize> {
        let mut admitted = vec![false; slots.len()];
        let mut spent: u32 = 0;
        // Per-lane spend tally — tracks how much of each lane's floor
        // has already been claimed.
        let mut lane_spent: HashMap<&str, u32> = HashMap::new();

        // Pass 1 — floor reservations, bypass global ceiling.
        for (idx, slot) in slots.iter().enumerate() {
            let lane = slot.lane.as_str();
            let floor = self.per_lane_floor.get(lane).copied().unwrap_or(0);
            if floor == 0 {
                continue;
            }
            let used = *lane_spent.get(lane).unwrap_or(&0);
            if used >= floor {
                continue;
            }
            admitted[idx] = true;
            spent = spent.saturating_add(slot.tokens);
            *lane_spent.entry(lane).or_insert(0) += slot.tokens;
        }

        // Pass 2 — greedy fill of remaining global budget. If the
        // floor pass overshot the ceiling (extreme case — can happen
        // when floors sum > max_tokens), pass 2 admits nothing but
        // pass 1's reservations stand. Starvation is the worse bug.
        for (idx, slot) in slots.iter().enumerate() {
            if admitted[idx] {
                continue;
            }
            if spent.saturating_add(slot.tokens) > self.max_tokens {
                continue;
            }
            admitted[idx] = true;
            spent = spent.saturating_add(slot.tokens);
        }

        admitted
            .into_iter()
            .enumerate()
            .filter_map(|(i, keep)| if keep { Some(i) } else { None })
            .collect()
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self::v2_default()
    }
}

/// Input row for `TokenBudget::apply`.
#[derive(Debug, Clone)]
pub struct Slot {
    pub lane: String,
    pub tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(lane: &str, t: u32) -> Slot {
        Slot {
            lane: lane.into(),
            tokens: t,
        }
    }

    #[test]
    fn default_budget_has_expected_floors() {
        let b = TokenBudget::v2_default();
        assert_eq!(b.max_tokens, 4096);
        assert_eq!(b.per_lane_floor.get("graph").copied(), Some(200));
        assert_eq!(b.per_lane_floor.get("symbol").copied(), Some(400));
        assert_eq!(b.per_lane_floor.get("lexical").copied(), Some(400));
        assert_eq!(b.per_lane_floor.get("fts").copied(), Some(400));
        assert_eq!(b.per_lane_floor.get("checkpoint").copied(), Some(200));
    }

    #[test]
    fn floor_admits_top_item_per_lane_even_under_skew() {
        // Pathological: symbol lane would be starved if lexical hogged
        // the global ceiling.
        let b = TokenBudget {
            max_tokens: 500,
            per_lane_floor: {
                let mut m = HashMap::new();
                m.insert("symbol", 100);
                m.insert("lexical", 100);
                m
            },
        };
        let slots = vec![
            slot("lexical", 400),
            slot("lexical", 400),
            slot("symbol", 50), // admitted via floor
        ];
        let kept = b.apply(&slots);
        // Lexical[0] admitted (floor), symbol[2] admitted (floor).
        // Lexical[1] rejected: greedy fill has only 100 left after
        // reserves (100 of 400 for lexical floor is already exceeded;
        // remaining budget = 500 - 400 - 50 = 50, so 400 won't fit).
        assert!(kept.contains(&0));
        assert!(kept.contains(&2));
        assert!(!kept.contains(&1));
    }

    #[test]
    fn no_lane_starves_under_pathological_weights() {
        // All lanes present; weight distribution assumes lexical would
        // otherwise crowd out symbol and graph entirely.
        let b = TokenBudget::v2_default();
        // Build 20 lexical items (100 tokens each) + 1 symbol + 1 graph.
        let mut slots = Vec::new();
        for _ in 0..20 {
            slots.push(slot("lexical", 100));
        }
        slots.push(slot("symbol", 80));
        slots.push(slot("graph", 60));

        let kept = b.apply(&slots);

        let lanes_kept: std::collections::HashSet<&str> =
            kept.iter().map(|&i| slots[i].lane.as_str()).collect();
        assert!(lanes_kept.contains("lexical"), "lexical must survive");
        assert!(lanes_kept.contains("symbol"), "symbol must not be starved");
        assert!(lanes_kept.contains("graph"), "graph must not be starved");
    }

    #[test]
    fn floor_pass_overshoot_still_keeps_reservations() {
        // Floors sum > max_tokens. Floor pass wins.
        let b = TokenBudget {
            max_tokens: 100,
            per_lane_floor: {
                let mut m = HashMap::new();
                m.insert("a", 80);
                m.insert("b", 80);
                m
            },
        };
        let slots = vec![slot("a", 80), slot("b", 80)];
        let kept = b.apply(&slots);
        // Both admitted via floor despite blowing past max.
        assert_eq!(kept, vec![0, 1]);
    }

    #[test]
    fn unknown_lane_gets_no_floor_but_can_fill_remainder() {
        let b = TokenBudget {
            max_tokens: 300,
            per_lane_floor: {
                let mut m = HashMap::new();
                m.insert("symbol", 100);
                m
            },
        };
        let slots = vec![
            slot("symbol", 50), // floor admitted
            slot("other", 100), // fills remainder
            slot("other", 200), // rejected — would push over 300
        ];
        let kept = b.apply(&slots);
        assert_eq!(kept, vec![0, 1]);
    }

    #[test]
    fn stable_order_preserved_in_output() {
        let b = TokenBudget {
            max_tokens: 1000,
            per_lane_floor: HashMap::new(),
        };
        let slots = vec![
            slot("a", 100),
            slot("b", 100),
            slot("c", 100),
            slot("d", 100),
        ];
        let kept = b.apply(&slots);
        // All admitted, original order preserved.
        assert_eq!(kept, vec![0, 1, 2, 3]);
    }
}
