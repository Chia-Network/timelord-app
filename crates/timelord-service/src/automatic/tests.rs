use super::*;
fn frequencies() -> Vec<f64> {
    (20..=160).map(|i| f64::from(i) * 10.).collect()
}
fn settings() -> AutomaticConfig {
    AutomaticConfig {
        healthy_seconds: 30,
        cooldown_seconds: 30,
        recovery_window_seconds: 60,
        ..Default::default()
    }
}
fn input() -> Input {
    Input {
        exploration_allowed: true,
        healthy: true,
        recoveries: 0,
        score: Some(950000.),
        temperature: Some(40.),
        temperature_limit: 55.,
        paused: false,
    }
}
fn policy() -> Policy {
    Policy::new(
        settings(),
        Point {
            frequency_mhz: 1450.,
            voltage_v: 1.,
        },
        0,
        frequencies(),
    )
}
fn capped_retest_policy() -> Policy {
    let mut config = settings();
    config.retest_after_seconds = 60;
    let mut p = Policy::new(
        config,
        Point {
            frequency_mhz: 1450.,
            voltage_v: 1.,
        },
        0,
        frequencies(),
    );
    p.ceiling = 1459.;
    p.baseline = Some(950000.);
    p.status.active_seconds = 30;
    p
}
fn start_retest(p: &mut Policy, now: u64) {
    p.retest.remaining_seconds = 0;
    let Some(Action::Set(target, reason)) = p.observe(now, input()) else {
        panic!("missing periodic retry")
    };
    assert_eq!(
        target,
        Point {
            frequency_mhz: 1460.,
            voltage_v: 1.
        }
    );
    assert!(p.is_retest());
    p.commit_retest();
    p.applied(target, now, reason.message);
}
#[test]
fn failed_retry_reservation_restores_attempt_and_wait_without_relaxing_ceiling() {
    let mut p = capped_retest_policy();
    p.retest.remaining_seconds = 0;
    let before = p.retest_state();
    let point = p.point;
    let ceiling = p.ceiling;
    assert!(matches!(p.observe(1, input()), Some(Action::Set(..))));
    assert_eq!(p.retest.attempts, before.attempts + 1);
    p.interrupt_trial(1); // Optional storage rejected the proposed reservation.
    assert_eq!(p.retest, before);
    assert_eq!(p.point, point);
    assert_eq!(p.ceiling, ceiling);
    assert!(!p.in_trial());
    // Once durable, cancellation of a real retry cannot erase its reservation.
    p.baseline = Some(950000.);
    p.status.active_seconds = 30;
    assert!(matches!(p.observe(2, input()), Some(Action::Set(..))));
    p.commit_retest();
    let durable = p.retest_state();
    p.interrupt_trial(2);
    assert_eq!(p.retest, durable);
}
#[test]
fn retests_only_after_healthy_cool_work_and_honors_disable_and_limits() {
    let mut p = capped_retest_policy();
    let mut paused = input();
    paused.paused = true;
    assert!(p.observe(1, paused).is_none());
    let remaining = p.retest.remaining_seconds;
    let mut idle = input();
    idle.healthy = false;
    assert!(p.observe(10000, idle).is_none());
    assert_eq!(p.retest.remaining_seconds, remaining);
    for now in 10001..10100 {
        assert!(!matches!(
            p.observe(
                now,
                Input {
                    exploration_allowed: true,
                    temperature: Some(51.),
                    ..input()
                }
            ),
            Some(Action::Set(..))
        ));
    }
    assert_eq!(p.retest.remaining_seconds, remaining);
    let mut missing_score = input();
    missing_score.score = None;
    assert!(p.observe(10100, missing_score).is_none());
    assert_eq!(p.retest.remaining_seconds, remaining);
    // Separate stable run: ordinary recoveries do not prohibit a retry.
    p = capped_retest_policy();
    for now in 1..60 {
        let mut sample = input();
        sample.recoveries = now / 20;
        assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
    }
    let mut sample = input();
    sample.recoveries = 3;
    let Some(Action::Set(target, _)) = p.observe(60, sample) else {
        panic!("retry never became due")
    };
    assert_eq!(target.frequency_mhz, 1460.);
    assert_eq!(target.voltage_v, 1.);
    assert!(p.is_retest());
    assert_eq!(p.retest.attempts, 1);
    assert_eq!(p.retest.remaining_seconds, 120);
    for disabled in [true, false] {
        let mut p = capped_retest_policy();
        if disabled {
            p.settings.retest_after_seconds = 0;
        } else {
            p.settings.max_frequency_mhz = 1450.;
        }
        p.retest.remaining_seconds = 0;
        for now in 1..200 {
            assert!(!matches!(p.observe(now, input()), Some(Action::Set(..))));
        }
    }
}
#[test]
fn unsupported_gap_below_configured_maximum_has_no_retry_countdown() {
    let mut p = capped_retest_policy();
    p.settings.max_frequency_mhz = 1455.;
    p.ceiling = 1450.;
    assert_eq!(p.retest_remaining(), None);
}
#[test]
fn failed_retests_roll_back_and_exponentially_delay_the_next_trial() {
    let mut p = capped_retest_policy();
    for attempt in 1..=12 {
        let now = attempt * 100;
        start_retest(&mut p, now);
        let mut storm = input();
        storm.recoveries = attempt * 20;
        let Some(Action::Set(target, _)) = p.observe(now + 6, storm) else {
            panic!("storm was not rejected")
        };
        assert_eq!(target.frequency_mhz, 1450.);
        assert!(!p.in_trial());
        assert_eq!(p.ceiling, 1450.);
        assert_eq!(
            p.retest.remaining_seconds,
            (60 * (1u64 << attempt)).min(86400)
        );
        p.applied(target, now + 6, "rollback".into());
        p.baseline = Some(950000.);
        p.status.active_seconds = 30;
        p.status.cooldown_seconds = 0;
        // Keep the next observation's recovery counter synchronized.
        p.synchronize_recoveries(0);
    }
}
#[test]
fn successful_retest_requires_the_full_recovery_window_before_reopening_search() {
    let mut p = capped_retest_policy();
    start_retest(&mut p, 1);
    for now in 2..121 {
        assert!(!matches!(p.observe(now, input()), Some(Action::Save)));
        assert!(p.is_retest());
        assert!(p.ceiling < p.point.frequency_mhz);
    }
    assert!(matches!(p.observe(121, input()), Some(Action::Save)));
    assert!(!p.is_retest());
    assert_eq!(p.ceiling, p.settings.max_frequency_mhz);
    assert_eq!(p.retest.attempts, 0);
}
#[test]
fn pause_during_retest_keeps_probation_and_the_previous_rollback_point() {
    let mut p = capped_retest_policy();
    start_retest(&mut p, 1);
    let mut paused = input();
    paused.paused = true;
    assert!(p.observe(20, paused).is_none());
    assert!(p.is_retest());
    assert_eq!(p.trial.as_ref().unwrap().rollback.frequency_mhz, 1450.);
    for now in 21..100 {
        assert!(!matches!(p.observe(now, input()), Some(Action::Save)));
    }
    let mut storm = input();
    storm.recoveries = 20;
    let Some(Action::Set(target, _)) = p.observe(101, storm) else {
        panic!("missing rollback")
    };
    assert_eq!(target.frequency_mhz, 1450.);
}
#[test]
fn retry_wait_is_serialized_before_the_trial_and_old_profiles_get_a_fresh_wait() {
    let mut p = capped_retest_policy();
    p.retest.remaining_seconds = 0;
    assert!(matches!(p.observe(1, input()), Some(Action::Set(..))));
    // This is the pre-command cache point: no unconfirmed target is saved.
    assert_eq!(p.point.frequency_mhz, 1450.);
    let mut entry: Learned = serde_json::from_value(serde_json::json!({
        "serial":"fixture", "engines":1, "engine_mask":2, "host":"mac", "client_sha256":"hash",
        "point":p.point, "active_seconds":0, "updated_unix":1,
        "ceiling_mhz":p.ceiling, "cooldown_seconds":0
    }))
    .unwrap();
    let mut old = capped_retest_policy();
    old.restore(&entry);
    assert_eq!(old.retest.remaining_seconds, 60);
    entry.retest = Some(p.retest_state());
    let saved: Learned = serde_json::from_slice(&serde_json::to_vec(&entry).unwrap()).unwrap();
    let mut restarted = capped_retest_policy();
    restarted.restore(&saved);
    assert_eq!(restarted.retest.remaining_seconds, 120);
    assert_eq!(restarted.retest.attempts, 1);
    let mut idle = input();
    idle.healthy = false;
    assert!(restarted.observe(100000, idle).is_none());
    assert_eq!(restarted.retest.remaining_seconds, 120);
    let remaining = restarted.retest.remaining_seconds;
    assert!(restarted.observe(0, input()).is_none());
    assert_eq!(restarted.retest.remaining_seconds, remaining);
    assert!(!restarted.is_retest());
}
#[test]
fn sparse_pll_steps_skip_unsupported_requests_and_respect_soft_ceiling() {
    let point = Point {
        frequency_mhz: 1500.,
        voltage_v: 1.,
    };
    let supported = vec![1480., 1490., 1500., 1520., 1540.];
    let mut p = Policy::new(settings(), point, 0, supported.clone());
    let mut next = None;
    for now in 1..=40 {
        if let Some(Action::Set(target, _)) = p.observe(now, input()) {
            next = Some(target);
            break;
        }
    }
    assert_eq!(next.unwrap().frequency_mhz, 1520.);
    p.applied(next.unwrap(), 41, "trial".into());
    let mut recovery = input();
    recovery.recoveries = 1;
    assert!(
        p.observe(42, recovery).is_none(),
        "One recovery must not reject a supported trial"
    );
    assert_eq!(p.point.frequency_mhz, 1520.);
    let mut config = settings();
    config.max_frequency_mhz = 1510.;
    let mut limited = Policy::new(config, point, 0, supported.clone());
    for now in 1..=100 {
        assert!(!matches!(
            limited.observe(now, input()),
            Some(Action::Set(..))
        ));
    }
    let mut backoff = Policy::new(
        settings(),
        Point {
            frequency_mhz: 1540.,
            voltage_v: 1.,
        },
        0,
        supported,
    );
    let mut thermal = input();
    thermal.temperature = Some(54.);
    let Some(Action::Set(target, _)) = backoff.observe(1, thermal) else {
        panic!("missing thermal backoff")
    };
    assert_eq!(target.frequency_mhz, 1520.);
}

#[test]
fn idle_and_unattributed_proofs_cannot_promote_a_setting() {
    let mut p = policy();
    for now in 0..300 {
        let mut i = input();
        i.healthy = false;
        assert!(p.observe(now, i).is_none());
    }
    assert_eq!(p.status.active_seconds, 0);
    for now in 300..500 {
        let mut i = input();
        i.score = None;
        assert!(p.observe(now, i).is_none());
    }
    assert_eq!(p.point.frequency_mhz, 1450.);
    use timelord_core::Chain;
    let observations: Vec<_> = (0..3)
        .flat_map(|_| [Chain::Challenge, Chain::Reward])
        .enumerate()
        .map(|(n, chain)| crate::runtime::Observation {
            id: n as u64,
            chain,
            timestamp_unix: 200 + n as u64,
            work_started_unix: 50,
            ips: 950000.,
        })
        .collect();
    assert_eq!(
        phase_score(
            observations.iter(),
            &[Chain::Challenge, Chain::Reward],
            100,
            3
        ),
        None
    );
    let fresh: Vec<_> = observations
        .into_iter()
        .map(|mut o| {
            o.work_started_unix = 120;
            o.ips = if o.chain == Chain::Reward {
                900000.
            } else {
                1000000.
            };
            o
        })
        .collect();
    assert_eq!(
        phase_score(fresh.iter(), &[Chain::Challenge, Chain::Reward], 100, 3),
        Some(900000.)
    );
    assert_eq!(
        phase_score(
            fresh.iter(),
            &[Chain::Challenge, Chain::Reward, Chain::InfusedChallenge],
            100,
            3
        ),
        None
    );
}
fn raised_trial() -> Policy {
    let mut p = policy();
    for now in (5..=35).step_by(5) {
        if let Some(Action::Set(point, reason)) = p.observe(now, input()) {
            p.applied(point, now, reason.message);
        }
    }
    assert_eq!(p.point.frequency_mhz, 1460.);
    p
}

#[test]
fn storm_rejects_a_trial_without_a_score_or_fresh_proofs() {
    let mut p = raised_trial();
    let mut sample = input();
    sample.healthy = false;
    sample.score = None;
    sample.recoveries = 19;
    assert!(p.observe(40, sample).is_none());
    let mut storm = input();
    storm.healthy = false;
    storm.score = None;
    storm.recoveries = 20;
    let Some(Action::Set(target, reason)) = p.observe(40, storm) else {
        panic!("storm must not wait for a score");
    };
    assert_eq!(target.frequency_mhz, 1450.);
    assert!(reason.message.contains("Recovery storm"));
    assert_eq!(p.learned_limits(), (1450., 30));
    p.applied(target, 40, reason.message);
    p.synchronize_recoveries(20);
    let mut settled = input();
    settled.recoveries = 20;
    settled.score = None;
    assert!(
        p.observe(45, settled).is_none(),
        "Do not count the previous point twice"
    );
}

#[test]
fn recorded_1920_burst_backs_off_in_five_seconds() {
    // First six one-second bins from ASIC 2's October 22 20:34 CDT trial.
    let counts = [14, 3, 2, 5, 5, 82];
    let point = Point {
        frequency_mhz: 1920.,
        voltage_v: 1.,
    };
    let mut p = Policy::new(
        AutomaticConfig::default(),
        point,
        0,
        vec![1880., 1900., 1920., 1940.],
    );
    let mut total = 0;
    for (now, count) in counts.into_iter().enumerate() {
        for _ in 0..count {
            total += 1;
            let mut sample = input();
            sample.healthy = false;
            sample.score = None;
            sample.recoveries = total;
            if let Some(Action::Set(target, _)) = p.observe(now as u64, sample) {
                assert_eq!(now, 5);
                assert_eq!(
                    target,
                    Point {
                        frequency_mhz: 1900.,
                        voltage_v: 1.
                    }
                );
                assert_eq!(total, 49);
                assert!(p.recovery_burst.events.len() <= STORM_RECOVERIES);
                return;
            }
        }
    }
    panic!("recorded storm was not rejected");
}

#[test]
fn rolling_storm_window_expires_and_memory_is_bounded() {
    let mut p = policy();
    let mut sample = input();
    sample.score = None;
    sample.recoveries = 19;
    assert!(p.observe(5, sample).is_none());
    let mut next = input();
    next.score = None;
    next.recoveries = 20;
    assert!(
        p.observe(15, next).is_none(),
        "Recoveries ten seconds apart are not a burst"
    );
    let mut burst = RecoveryBurst::default();
    assert!(burst.observe(10, u64::MAX));
    assert_eq!(burst.events.len(), STORM_RECOVERIES);
    assert!(!burst.observe(20, 0));
}

#[test]
fn pause_counter_reset_and_clock_reversal_do_not_create_storms() {
    let mut p = policy();
    let mut paused = input();
    paused.paused = true;
    paused.recoveries = 100;
    assert!(p.observe(5, paused).is_none());
    let mut resumed = input();
    resumed.recoveries = 200;
    resumed.score = None;
    assert!(p.observe(6, resumed).is_none());
    let mut unchanged = input();
    unchanged.recoveries = 200;
    unchanged.score = None;
    assert!(p.observe(11, unchanged).is_none());
    let mut reset = input();
    reset.score = None;
    reset.recoveries = 0;
    assert!(p.observe(12, reset).is_none());
    let mut nineteen = input();
    nineteen.score = None;
    nineteen.recoveries = 19;
    assert!(p.observe(20, nineteen).is_none());
    let mut backwards = input();
    backwards.score = None;
    backwards.recoveries = 20;
    assert!(p.observe(19, backwards).is_none());
    let mut later = input();
    later.score = None;
    later.recoveries = 20;
    assert!(p.observe(24, later).is_none());
}

#[test]
fn storm_forces_lower_frequency_even_at_lower_voltage_and_during_cooldown() {
    let mut p = policy();
    p.point.voltage_v = 0.95;
    p.status.cooldown_seconds = 30;
    let mut sample = input();
    sample.recoveries = 20;
    sample.score = None;
    let Some(Action::Set(target, reason)) = p.observe(5, sample) else {
        panic!("missing fast backoff");
    };
    assert!(target.frequency_mhz < p.point.frequency_mhz);
    assert_eq!(target.voltage_v, 0.95);
    p.applied(target, 5, reason.message);
    p.synchronize_recoveries(20);
    let mut old_logs = input();
    old_logs.recoveries = 150;
    old_logs.score = None;
    assert!(p.observe(6, old_logs).is_none());
    let mut no_new_errors = input();
    no_new_errors.recoveries = 150;
    no_new_errors.score = None;
    assert!(
        p.observe(10, no_new_errors).is_none(),
        "Old-point log backlog must not reject the new point"
    );
    let mut continued = input();
    continued.recoveries = 170;
    continued.score = None;
    let Some(Action::Set(lower, _)) = p.observe(10, continued) else {
        panic!("cooldown must not block protective backoff");
    };
    assert!(lower.frequency_mhz < target.frequency_mhz);
}

#[test]
fn storm_at_frequency_floor_does_not_repeat_a_hardware_write() {
    let mut p = policy();
    p.settings.min_frequency_mhz = p.point.frequency_mhz;
    let mut storm = input();
    storm.recoveries = 20;
    storm.score = None;
    assert!(p.observe(5, storm).is_none());
    assert!(p.status.message.contains("minimum frequency"));
}

#[test]
fn moderately_bursty_recoveries_with_good_performance_remain_allowed() {
    let mut p = policy();
    p.settings.max_frequency_mhz = p.point.frequency_mhz;
    p.ceiling = p.point.frequency_mhz;
    for now in (5..=3600).step_by(5) {
        let mut sample = input();
        sample.recoveries = now / 10 * 3;
        assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
    }
}

#[test]
fn normal_recoveries_with_good_ips_do_not_reduce_frequency_or_voltage_over_hours() {
    let mut config = settings();
    config.max_frequency_mhz = 1450.;
    let mut p = Policy::new(config, policy().point, 0, frequencies());
    for now in (5..=14400).step_by(5) {
        let mut sample = input();
        sample.recoveries = now / 50;
        assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
        assert_eq!(
            p.point,
            Point {
                frequency_mhz: 1450.,
                voltage_v: 1.
            }
        );
    }
    assert_eq!(p.status.state, "monitoring");
}

#[test]
fn recovered_faster_trial_is_accepted_but_two_degraded_windows_back_off() {
    let mut faster = raised_trial();
    for now in (40..=65).step_by(5) {
        let mut sample = input();
        sample.recoveries = 2;
        sample.score = Some(970000.);
        assert!(!matches!(
            faster.observe(now, sample),
            Some(Action::Set(..))
        ));
    }
    assert!(!faster.in_trial());
    assert_eq!(faster.point.frequency_mhz, 1460.);
    let mut p = raised_trial();
    for now in (40..=150).step_by(5) {
        let mut sample = input();
        sample.recoveries = 1 + (now - 40) / 60;
        sample.score = Some(850000.);
        assert!(
            !matches!(p.observe(now, sample), Some(Action::Set(..))),
            "premature backoff at {now}"
        );
    }
    let mut sample = input();
    sample.recoveries = 2;
    sample.score = Some(850000.);
    let Some(Action::Set(target, reason)) = p.observe(155, sample) else {
        panic!("missing recovery-cost backoff")
    };
    assert_eq!(
        target,
        Point {
            frequency_mhz: 1450.,
            voltage_v: 1.
        }
    );
    assert!(reason.message.contains("consecutive"));
    assert_eq!(p.status.cooldown_seconds, 30);
}

#[test]
fn one_bad_window_then_recovery_keeps_the_higher_setting() {
    let mut p = raised_trial();
    p.ceiling = 1460.;
    for now in (40..=95).step_by(5) {
        let mut sample = input();
        sample.recoveries = 1;
        sample.score = Some(850000.);
        assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
    }
    assert_eq!(p.degraded_windows, 1);
    for now in (100..=150).step_by(5) {
        let mut good = input();
        good.recoveries = 1;
        good.score = Some(970000.);
        assert!(p.observe(now, good).is_none());
    }
    let mut good = input();
    good.recoveries = 1;
    good.score = Some(970000.);
    assert!(matches!(p.observe(155, good), Some(Action::Save)));
    assert_eq!(p.degraded_windows, 0);
    assert_eq!(p.point.frequency_mhz, 1460.);
}

#[test]
fn flat_performance_and_pause_do_not_restore_a_lower_setting() {
    let mut p = raised_trial();
    for now in (40..=65).step_by(5) {
        assert!(!matches!(p.observe(now, input()), Some(Action::Set(..))));
    }
    assert!(!p.in_trial());
    assert_eq!(p.point.frequency_mhz, 1460.);
    let mut p = raised_trial();
    let mut paused = input();
    paused.paused = true;
    assert!(p.observe(40, paused).is_none());
    assert_eq!(p.point.frequency_mhz, 1460.);
    for now in (45..=100).step_by(5) {
        let mut sample = input();
        sample.paused = true;
        assert!(p.observe(now, sample).is_none());
    }
    assert!(p.observe(105, input()).is_none());
    assert_eq!(p.status.active_seconds, 5);
}

#[test]
fn missing_gain_without_recoveries_is_not_a_hardware_failure_and_heat_still_backs_off() {
    let mut p = raised_trial();
    p.ceiling = 1460.;
    for now in (40..=400).step_by(5) {
        let mut sample = input();
        sample.score = Some(800000.);
        assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
    }
    assert_eq!(p.point.frequency_mhz, 1460.);
    let mut thermal = input();
    thermal.temperature = Some(54.);
    let Some(Action::Set(target, _)) = p.observe(405, thermal) else {
        panic!("thermal backoff missing")
    };
    assert!(target.frequency_mhz < p.point.frequency_mhz);
    assert_eq!(target.voltage_v, 1.);
    let mut warm = policy();
    for now in (5..=120).step_by(5) {
        let mut sample = input();
        sample.temperature = Some(53.);
        assert!(!matches!(warm.observe(now, sample), Some(Action::Set(..))));
    }
}

#[test]
fn sustained_recovery_loss_at_lower_voltage_raises_voltage_before_lowering_frequency() {
    let mut config = settings();
    config.max_frequency_mhz = 1450.;
    let mut p = Policy::new(
        config,
        Point {
            frequency_mhz: 1450.,
            voltage_v: 0.95,
        },
        0,
        frequencies(),
    );
    for now in (5..=30).step_by(5) {
        p.observe(now, input());
    }
    for now in (35..=145).step_by(5) {
        let mut sample = input();
        sample.recoveries = 1 + (now - 35) / 60;
        sample.score = Some(850000.);
        assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
    }
    let mut sample = input();
    sample.recoveries = 2;
    sample.score = Some(850000.);
    let Some(Action::Set(target, _)) = p.observe(150, sample) else {
        panic!("missing voltage restoration")
    };
    assert_eq!(
        target,
        Point {
            frequency_mhz: 1450.,
            voltage_v: 1.
        }
    );
}

#[test]
fn recovery_yields_a_trial_without_lowering_a_healthy_clock_or_learning_it_early() {
    let mut p = capped_retest_policy();
    start_retest(&mut p, 1);
    let current = p.point;
    p.interrupt_trial(2);
    assert_eq!(p.point, current);
    assert!(!p.in_trial());
    assert!(p.baseline.is_none());
    assert_eq!(p.status.active_seconds, 0);
    for now in 3..=31 {
        let mut i = input();
        i.exploration_allowed = false;
        assert!(p.observe(now, i).is_none());
    }
    let mut i = input();
    i.exploration_allowed = false;
    assert!(matches!(p.observe(32, i), Some(Action::Save)));
    for now in 33..=400 {
        let mut i = input();
        i.exploration_allowed = false;
        assert!(p.observe(now, i).is_none());
    }
    assert_eq!(p.point, current);
    assert!(!p.in_trial());
    let mut i = input();
    i.exploration_allowed = false;
    i.temperature = Some(54.5);
    assert!(
        matches!(p.observe(401,i),Some(Action::Set(point,_)) if point.frequency_mhz<current.frequency_mhz)
    );
}
