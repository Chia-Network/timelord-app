//! Chain routing over board reliability. Pure policy: no I/O or locks.
//!
//! - Every serving, enabled board carries work: a working board is never left
//!   idle on suspicion. Only hardware evidence (a stopped owner, a safety
//!   restriction) or an operator disable takes a board out.
//! - Reliability decides which chain each board carries: the most reliable
//!   boards get the challenge and reward chains, a board on probation the
//!   infused-challenge chain.
//! - Layouts stay 3 / 2+1 / 1+1+1 engines over the boards in use.
//! - Between peaks a chain stays on its board while that board serves;
//!   improvements wait for `allow_rebalance` (a new peak) so they never cancel
//!   in-flight work.
use super::health::Tier;
use timelord_core::{Assignment, Chain};

#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub tier: Tier,
    /// The native owner passed its checks and accepts work.
    pub serving: bool,
    pub failures_last_hour: usize,
}

const CHAINS: [Chain; 3] = [Chain::Challenge, Chain::Reward, Chain::InfusedChallenge];

fn eligible(candidates: &[Candidate]) -> Vec<(usize, &Candidate)> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.serving && c.tier != Tier::Disabled)
        .collect()
}

fn layout_valid(boards: &[usize]) -> bool {
    let mut counts = std::collections::BTreeMap::new();
    for board in boards {
        *counts.entry(*board).or_insert(0) += 1;
    }
    let mut counts: Vec<_> = counts.into_values().collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    matches!(counts.as_slice(), [3] | [2, 1] | [1, 1, 1])
}

/// Keep a chain's physical engine when it stays on its board and that engine is
/// part of the board's new engine set; assign the rest to the remaining slots.
fn engines(plan: &mut [Assignment], previous: &[Assignment]) {
    let mut slots: Vec<(String, u8)> = vec![];
    for board in plan.iter().map(|a| a.board_id.clone()) {
        if slots.iter().any(|(b, _)| *b == board) {
            continue;
        }
        let count = plan.iter().filter(|a| a.board_id == board).count();
        let set: &[u8] = match count {
            1 => &[1],
            2 => &[0, 1],
            _ => &[0, 1, 2],
        };
        slots.extend(set.iter().map(|&e| (board.clone(), e)));
    }
    let mut used = std::collections::HashSet::new();
    let mut retained = vec![];
    for assignment in plan.iter_mut() {
        if let Some(old) = previous
            .iter()
            .find(|old| old.chain == assignment.chain && old.board_id == assignment.board_id)
            && slots.contains(&(old.board_id.clone(), old.engine))
            && used.insert((old.board_id.clone(), old.engine))
        {
            assignment.engine = old.engine;
            retained.push(assignment.chain);
        }
    }
    for assignment in plan.iter_mut().filter(|a| !retained.contains(&a.chain)) {
        let engine = slots
            .iter()
            .find(|(board, engine)| {
                *board == assignment.board_id && !used.contains(&(board.clone(), *engine))
            })
            .map(|(_, engine)| *engine)
            .expect("a valid layout has a free engine for every chain");
        assignment.engine = engine;
        used.insert((assignment.board_id.clone(), engine));
    }
}

/// Plan routes. Returns an empty plan when no board can serve.
pub fn plan(
    candidates: &[Candidate],
    previous: &[Assignment],
    allow_rebalance: bool,
) -> Vec<Assignment> {
    let eligible = eligible(candidates);
    if eligible.is_empty() {
        return vec![];
    }
    let index = |id: &str| eligible.iter().position(|(_, c)| c.id == id);
    let previous_boards: Option<Vec<usize>> = CHAINS
        .iter()
        .map(|chain| {
            previous
                .iter()
                .find(|a| a.chain == *chain)
                .and_then(|a| index(&a.board_id))
        })
        .collect();
    // Between peaks, keep a complete plan whose boards all still serve. A
    // newly serving board joins at the next peak.
    if !allow_rebalance
        && let Some(boards) = &previous_boards
        && layout_valid(boards)
    {
        let mut kept: Vec<_> = CHAINS
            .iter()
            .map(|chain| {
                previous
                    .iter()
                    .find(|a| a.chain == *chain)
                    .cloned()
                    .expect("previous route exists")
            })
            .collect();
        engines(&mut kept, previous);
        return kept;
    }
    let n = eligible.len();
    // Larger is better. Reliability of the board carrying each chain, in chain
    // priority order; order only breaks exact ties.
    let reliability = |board: usize| {
        let (_, c) = eligible[board];
        (
            std::cmp::Reverse(c.tier),
            std::cmp::Reverse(c.failures_last_hour),
        )
    };
    let score = |boards: &[usize]| {
        let used = boards
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let retained = CHAINS
            .iter()
            .zip(boards)
            .filter(|(chain, board)| {
                previous
                    .iter()
                    .any(|a| a.chain == **chain && a.board_id == eligible[**board].1.id)
            })
            .count();
        let order = |board: usize| std::cmp::Reverse(eligible[board].0);
        // At a peak, reliability beats retention.
        (
            used,
            (
                reliability(boards[0]),
                reliability(boards[1]),
                reliability(boards[2]),
            ),
            retained,
            order(boards[0]),
            order(boards[1]),
        )
    };
    // A forced replan between peaks moves as few chains as possible.
    let forced_score = |boards: &[usize]| {
        let used = boards
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let retained = CHAINS
            .iter()
            .zip(boards)
            .filter(|(chain, board)| {
                previous
                    .iter()
                    .any(|a| a.chain == **chain && a.board_id == eligible[**board].1.id)
            })
            .count();
        (
            used,
            retained,
            reliability(boards[0]),
            reliability(boards[1]),
            reliability(boards[2]),
            std::cmp::Reverse(eligible[boards[0]].0),
        )
    };
    let mut best: Option<Vec<usize>> = None;
    for a in 0..n {
        for b in 0..n {
            for c in 0..n {
                let boards = vec![a, b, c];
                if !layout_valid(&boards) {
                    continue;
                }
                let better = best.as_ref().is_none_or(|current| {
                    if allow_rebalance {
                        score(&boards) > score(current)
                    } else {
                        forced_score(&boards) > forced_score(current)
                    }
                });
                if better {
                    best = Some(boards);
                }
            }
        }
    }
    let Some(boards) = best else {
        return vec![];
    };
    let mut planned: Vec<_> = CHAINS
        .iter()
        .zip(boards)
        .map(|(chain, board)| Assignment {
            board_id: eligible[board].1.id.clone(),
            chain: *chain,
            engine: 0,
        })
        .collect();
    engines(&mut planned, previous);
    planned
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(id: &str, tier: Tier) -> Candidate {
        Candidate {
            id: id.into(),
            tier,
            serving: true,
            failures_last_hour: 0,
        }
    }
    fn board_of(plan: &[Assignment], chain: Chain) -> &str {
        &plan.iter().find(|a| a.chain == chain).unwrap().board_id
    }
    fn trio() -> Vec<Candidate> {
        vec![
            candidate("a", Tier::Trusted),
            candidate("b", Tier::Trusted),
            candidate("c", Tier::Trusted),
        ]
    }
    #[test]
    fn layouts_follow_usable_board_count() {
        let all = trio();
        let three = plan(&all, &[], true);
        assert_eq!(three.len(), 3);
        assert!(three.iter().all(|a| a.engine == 1));
        let two = plan(&all[..2], &[], true);
        assert_eq!(board_of(&two, Chain::Challenge), "a");
        assert_eq!(board_of(&two, Chain::Reward), "a");
        assert_eq!(board_of(&two, Chain::InfusedChallenge), "b");
        let one = plan(&all[..1], &[], true);
        let mut engines: Vec<_> = one.iter().map(|a| a.engine).collect();
        engines.sort();
        assert_eq!(engines, [0, 1, 2]);
        assert!(plan(&[], &three, true).is_empty());
    }
    #[test]
    fn every_serving_board_carries_work_and_reliability_only_picks_the_chain() {
        let mut boards = trio();
        boards[0].tier = Tier::Probation;
        let routes = plan(&boards, &[], true);
        assert_eq!(board_of(&routes, Chain::InfusedChallenge), "a");
        assert_ne!(board_of(&routes, Chain::Challenge), "a");
        assert_ne!(board_of(&routes, Chain::Reward), "a");
        // Two boards on probation: neither is left idle; the trusted board
        // takes the challenge chain.
        boards[1].tier = Tier::Probation;
        let routes = plan(&boards, &[], true);
        assert_eq!(board_of(&routes, Chain::Challenge), "c");
        for id in ["a", "b", "c"] {
            assert!(routes.iter().any(|r| r.board_id == id), "{id} left idle");
        }
        // Only an operator disable or a non-serving owner takes a board out.
        boards[1].tier = Tier::Disabled;
        boards[2].serving = false;
        let routes = plan(&boards, &[], true);
        assert!(routes.iter().all(|a| a.board_id == "a"));
        assert_eq!(routes.len(), 3);
        assert!(plan(&[candidate("x", Tier::Disabled)], &[], true).is_empty());
    }
    #[test]
    fn a_failing_board_moves_only_its_own_chain() {
        let all = trio();
        let initial = plan(&all, &[], true);
        let lost = board_of(&initial, Chain::Reward).to_owned();
        let mut remaining = all.clone();
        remaining.retain(|c| c.id != lost);
        let next = plan(&remaining, &initial, false);
        for chain in [Chain::Challenge, Chain::InfusedChallenge] {
            assert_eq!(board_of(&next, chain), board_of(&initial, chain));
        }
        assert_ne!(board_of(&next, Chain::Reward), lost);
        // The challenge chain keeps its physical engine on the board that now
        // carries two chains.
        let cc_board = board_of(&next, Chain::Challenge).to_owned();
        let old_engine = initial
            .iter()
            .find(|a| a.chain == Chain::Challenge)
            .unwrap()
            .engine;
        if next.iter().filter(|a| a.board_id == cc_board).count() == 2 {
            assert_eq!(
                next.iter()
                    .find(|a| a.chain == Chain::Challenge)
                    .unwrap()
                    .engine,
                old_engine
            );
        }
    }
    #[test]
    fn a_returning_board_joins_at_a_peak_and_takes_the_infused_chain_on_probation() {
        let all = trio();
        let degraded = plan(&all[1..], &[], true);
        let mut returning = all.clone();
        returning[0].tier = Tier::Probation;
        // Between peaks nothing moves.
        assert_eq!(plan(&returning, &degraded, false), degraded);
        // At a peak the probation board takes only the infused chain, the
        // challenge chain stays put and the layout spreads to one engine each.
        let rebalanced = plan(&returning, &degraded, true);
        assert_eq!(board_of(&rebalanced, Chain::InfusedChallenge), "a");
        assert_eq!(
            board_of(&rebalanced, Chain::Challenge),
            board_of(&degraded, Chain::Challenge)
        );
        assert_ne!(board_of(&rebalanced, Chain::Reward), "a");
        assert!(rebalanced.iter().all(|a| a.engine == 1));
    }
    #[test]
    fn board_order_alone_never_moves_a_healthy_chain() {
        let all = trio();
        let routes = plan(&all, &[], true);
        let reordered = vec![all[2].clone(), all[0].clone(), all[1].clone()];
        assert_eq!(plan(&reordered, &routes, true), routes);
    }
    #[test]
    fn critical_chains_prefer_boards_with_fewer_recent_failures() {
        let mut boards = trio();
        boards[0].failures_last_hour = 4;
        let routes = plan(&boards, &[], true);
        assert_eq!(board_of(&routes, Chain::InfusedChallenge), "a");
    }
    #[test]
    fn a_board_falling_to_probation_hands_over_its_critical_chain_at_a_peak() {
        let all = trio();
        let routes = plan(&all, &[], true);
        let holder = board_of(&routes, Chain::Challenge).to_owned();
        let mut changed = all.clone();
        changed.iter_mut().find(|c| c.id == holder).unwrap().tier = Tier::Probation;
        // Its work continues between peaks; the handover happens at a peak.
        assert_eq!(plan(&changed, &routes, false), routes);
        let next = plan(&changed, &routes, true);
        assert_ne!(board_of(&next, Chain::Challenge), holder);
        assert_eq!(board_of(&next, Chain::InfusedChallenge), holder);
    }
}

/// Randomized fault sequences over the real health and routing policy. Every
/// step must satisfy the routing invariants; the long variant is `#[ignore]`d.
#[cfg(test)]
mod chaos {
    use super::*;
    use crate::hardware::health::Health;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    struct Board {
        id: String,
        health: Health,
        serving: bool,
        down_until: u64,
        flaky: bool,
    }

    fn simulate(seed: u64, steps: u64) {
        let mut rng = Rng(seed | 1);
        let mut boards: Vec<_> = (0..3)
            .map(|i| Board {
                id: format!("b{i}"),
                health: Health::default(),
                serving: true,
                down_until: 0,
                flaky: i == 2,
            })
            .collect();
        let mut routes: Vec<Assignment> = vec![];
        let mut moves_by_healthy_failure = 0u64;
        for step in 0..steps {
            let now = 1_000_000 + step * 5;
            // Faults: the flaky board fails often, the others rarely.
            for board in &mut boards {
                if board.serving && rng.below(if board.flaky { 40 } else { 4000 }) == 0 {
                    board.serving = false;
                    board.health.record_failure(now, "chaos");
                    board.down_until = now + 5 * (1 + rng.below(6));
                } else if !board.serving && now >= board.down_until && !board.health.disabled(now) {
                    board.serving = true;
                }
                if board.serving && board.health.stable(now, now.saturating_sub(4000), now) {
                    board.health.clear_streak();
                }
            }
            if rng.below(500) == 0 {
                let target = rng.below(3) as usize;
                if boards[target].health.disabled(now) {
                    boards[target].health.enable();
                } else {
                    boards[target].health.disable(Some(now + 600));
                }
            }
            let allow = rng.below(4) == 0; // a new peak roughly every 20 s
            let candidates: Vec<_> = boards
                .iter()
                .map(|b| Candidate {
                    id: b.id.clone(),
                    tier: b.health.tier(now),
                    serving: b.serving,
                    failures_last_hour: b.health.failures_within(now, 3600),
                })
                .collect();
            let next = plan(&candidates, &routes, allow);
            let eligible = |id: &str| {
                candidates
                    .iter()
                    .any(|c| c.id == id && c.serving && c.tier != Tier::Disabled)
            };
            let count = candidates.iter().filter(|c| eligible(&c.id)).count();
            // 1. Only serving, enabled boards carry chains.
            assert!(
                next.iter().all(|a| eligible(&a.board_id)),
                "step {step}: {next:?}"
            );
            // 2. Every chain is covered whenever any board can serve.
            assert_eq!(next.len(), if count > 0 { 3 } else { 0 }, "step {step}");
            let used: std::collections::BTreeSet<_> =
                next.iter().map(|a| a.board_id.clone()).collect();
            if allow {
                // 3. At a peak no working board is left idle, and the
                // challenge chain is on the most reliable board in use.
                assert_eq!(used.len(), count.min(3), "step {step}: idle board");
                let rank = |id: &str| {
                    let c = candidates.iter().find(|c| c.id == id).unwrap();
                    (
                        std::cmp::Reverse(c.tier),
                        std::cmp::Reverse(c.failures_last_hour),
                    )
                };
                if let Some(cc) = next.iter().find(|a| a.chain == Chain::Challenge) {
                    assert!(
                        used.iter().all(|id| rank(&cc.board_id) >= rank(id)),
                        "step {step}"
                    );
                }
            }
            // 4. Layout and engines are valid.
            if !next.is_empty() {
                let boards_used: Vec<usize> = next
                    .iter()
                    .map(|a| candidates.iter().position(|c| c.id == a.board_id).unwrap())
                    .collect();
                assert!(layout_valid(&boards_used), "step {step}: {next:?}");
                let mut slots = std::collections::HashSet::new();
                assert!(
                    next.iter()
                        .all(|a| slots.insert((a.board_id.clone(), a.engine)))
                );
            }
            // 5. Between peaks a complete plan whose boards still serve never
            // moves.
            if !allow {
                let previous_valid =
                    routes.len() == 3 && routes.iter().all(|a| eligible(&a.board_id));
                if previous_valid {
                    assert_eq!(next, routes, "step {step}: optional move between peaks");
                }
            }
            for a in &next {
                if let Some(old) = routes.iter().find(|o| o.chain == a.chain)
                    && old.board_id != a.board_id
                    && !boards.iter().any(|b| b.id == old.board_id && b.flaky)
                {
                    moves_by_healthy_failure += 1;
                }
            }
            routes = next;
        }
        // Healthy boards' chains rarely move because of another board.
        assert!(moves_by_healthy_failure < steps / 10);
    }
    #[test]
    fn routing_invariants_hold_under_random_faults() {
        for seed in [1, 7, 42, 1234, 99991] {
            simulate(seed, 20_000);
        }
    }
    #[test]
    #[ignore = "long soak: cargo test -p timelord-service routing::chaos -- --ignored"]
    fn routing_invariants_hold_over_a_long_soak() {
        for seed in 1..=20 {
            simulate(seed * 2654435761, 500_000);
        }
    }
}
