use super::*;
use crate::event_handler::{AdmissionKind, AdmitOutcome};
use std::collections::HashSet;
use std::time::{Duration, Instant};

fn admit_for_late_maximize(press_count: Option<u64>, maximized: bool) -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = true;
    state.injected_press_count = press_count;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    assert_eq!(
        state.try_admit_window_at_with_native_ops(
            100,
            AdmissionKind::Automatic,
            None,
            |_| maximized,
            |_| {
                assert!(maximized, "unmaximized admission must not restore");
                Ok(true)
            },
        ),
        AdmitOutcome::Admitted
    );
    state.paused = false;
    state.layout_transition = None;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state
}

fn admit_unmaximized_for_late_maximize(press_count: Option<u64>) -> AppState {
    admit_for_late_maximize(press_count, false)
}

fn complete_late_maximize_restore(state: &mut AppState) {
    state.injected_window_maximized.insert(100, false);
    state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
        window_id: 100,
        managed_lifetime_token: state.managed_lifetime_tokens[&100],
        still_maximized: false,
    });
}

#[test]
fn test_release_or_mouse_move_after_admission_keeps_late_maximize_eligible() {
    let mut state = admit_unmaximized_for_late_maximize(Some(10));
    let queued = std::cell::Cell::new(0);
    for _ in 0..2 {
        state.on_window_moved_or_resized_with_native_ops(
            100,
            false,
            || Some(10),
            |_| {
                queued.set(queued.get() + 1);
                Ok(true)
            },
            |_| true,
        );
    }
    assert_eq!(queued.get(), 1);
    assert!(state.pending_maximized_admission_restores.contains(&100));
    assert!(state.window_last_maximized_at.contains_key(&100));
    let batches_before = state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .len();
    complete_late_maximize_restore(&mut state);
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert!(!state.window_last_maximized_at.contains_key(&100));
    assert!(state.focused_workspace().unwrap().columns()[0].contains(100));
    assert!(!state.focused_workspace().unwrap().is_floating(100));
    assert_eq!(
        state
            .injected_apply_placements_batches
            .lock()
            .unwrap()
            .len(),
        batches_before + 1
    );
    assert!(state.last_placed_layout_rects.contains_key(&100));
}

#[test]
fn test_late_maximize_after_new_press_or_unavailable_observers_is_allowed() {
    for (admission_count, press_count) in [
        (Some(10), Some(11)),
        (None, Some(10)),
        (Some(10), None),
        (None, None),
    ] {
        let mut state = admit_unmaximized_for_late_maximize(admission_count);
        let placements = state.last_placed_layout_rects.clone();
        state.on_window_moved_or_resized_with_native_ops(
            100,
            false,
            || press_count,
            |_| panic!("a deliberate maximize or unavailable observers must not restore"),
            |_| true,
        );
        assert!(!state.pending_maximized_admission_restores.contains(&100));
        assert!(state.window_last_maximized_at.contains_key(&100));
        assert_eq!(state.last_placed_layout_rects, placements);
        assert!(!state
            .post_admission_maximize_restore_eligible
            .contains_key(&100));
    }
}

#[test]
fn test_repeated_self_maximize_restores_at_most_four_times_including_admission() {
    for maximized_at_admission in [false, true] {
        let mut state = admit_for_late_maximize(Some(10), maximized_at_admission);
        let queued = std::cell::Cell::new(u8::from(maximized_at_admission));
        if maximized_at_admission {
            complete_late_maximize_restore(&mut state);
        }
        for expected in (u8::from(maximized_at_admission) + 1)..=4 {
            state.on_window_moved_or_resized_with_native_ops(
                100,
                false,
                || Some(10),
                |_| {
                    queued.set(queued.get() + 1);
                    Ok(true)
                },
                |_| true,
            );
            assert_eq!(queued.get(), expected);
            assert!(state.pending_maximized_admission_restores.contains(&100));
            complete_late_maximize_restore(&mut state);
            assert!(!state.window_last_maximized_at.contains_key(&100));
            assert!(state.last_placed_layout_rects.contains_key(&100));
        }
        state.injected_window_maximized.insert(100, true);
        state.moved_or_resized_suppression.remove(&100);
        for _ in 0..2 {
            state.on_window_moved_or_resized_with_native_ops(
                100,
                false,
                || Some(10),
                |_| panic!("the four-restore budget must stop further restores"),
                |_| true,
            );
            assert!(!state.pending_maximized_admission_restores.contains(&100));
            assert!(state.window_last_maximized_at.contains_key(&100));
        }
    }
}

#[test]
fn test_pending_remaximize_is_freshly_sampled_and_retried_on_completion() {
    let mut state = admit_for_late_maximize(Some(10), true);
    let token = state.managed_lifetime_tokens[&100];
    for restores_issued in 1..=4 {
        state.on_window_moved_or_resized_with_native_ops(
            100,
            false,
            || panic!("pending restore must not sample input"),
            |_| panic!("only one restore may be outstanding"),
            |_| panic!("pending notification must await completion"),
        );
        let queued = std::cell::Cell::new(0);
        state.on_maximized_admission_restored_with_native_ops(
            100,
            token,
            false,
            || Some(10),
            |_| {
                queued.set(queued.get() + 1);
                Ok(true)
            },
            |_| true,
        );
        assert_eq!(queued.get(), i32::from(restores_issued < 4));
        assert_eq!(
            state.pending_maximized_admission_restores.contains(&100),
            restores_issued < 4
        );
        assert!(state.window_last_maximized_at.contains_key(&100));
    }
}

#[test]
fn test_pending_remaximize_completion_respects_input_and_settle_cutoffs() {
    for (admission_count, press_count, expired) in [
        (Some(10), Some(11), false),
        (Some(10), None, false),
        (None, Some(10), false),
        (Some(10), Some(10), true),
    ] {
        let mut state = admit_for_late_maximize(admission_count, true);
        if expired {
            state.window_managed_at.insert(
                100,
                Instant::now() - crate::event_handler::SNAPBACK_SETTLE_AFTER_CREATE,
            );
        }
        state.on_maximized_admission_restored_with_native_ops(
            100,
            state.managed_lifetime_tokens[&100],
            false,
            || press_count,
            |_| panic!("completion must not restore past the input or settle cutoff"),
            |_| true,
        );
        assert!(!state.pending_maximized_admission_restores.contains(&100));
        assert!(state.window_last_maximized_at.contains_key(&100));
        state.on_window_moved_or_resized_with_native_ops(
            100,
            false,
            || press_count,
            |_| panic!("ended eligibility must not revive on later notifications"),
            |_| true,
        );
    }
}

#[test]
fn test_successful_completion_event_samples_for_a_later_remaximize() {
    for native_maximized in [false, true] {
        let mut state = admit_for_late_maximize(Some(10), true);
        state
            .injected_window_maximized
            .insert(100, native_maximized);
        state.injected_press_count = Some(11);
        state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
            window_id: 100,
            managed_lifetime_token: state.managed_lifetime_tokens[&100],
            still_maximized: false,
        });
        assert!(!state.pending_maximized_admission_restores.contains(&100));
        assert_eq!(
            state.window_last_maximized_at.contains_key(&100),
            native_maximized
        );
        assert_eq!(
            state.last_placed_layout_rects.contains_key(&100),
            !native_maximized
        );
    }
}

#[test]
fn test_unrestored_worker_verdict_does_not_chain_restore_for_zoomed_replacement() {
    let mut state = admit_for_late_maximize(Some(10), true);
    state.on_maximized_admission_restored_with_native_ops(
        100,
        state.managed_lifetime_tokens[&100],
        true,
        || Some(10),
        |_| panic!("an identity-skipped or failed restore must not chain another restore"),
        |_| true,
    );
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert!(state.window_last_maximized_at.contains_key(&100));
}

#[test]
fn test_unrestored_worker_verdict_preserves_iconic_window_grace_and_placed_rect() {
    let mut state = admit_for_late_maximize(Some(10), true);
    let placed_rect = Rect::new(20, 30, 400, 500);
    state.last_placed_layout_rects.insert(100, placed_rect);
    let batches_before = state
        .injected_apply_placements_batches
        .lock()
        .unwrap()
        .len();
    let before_completion = Instant::now();
    state.on_maximized_admission_restored_with_native_ops(
        100,
        state.managed_lifetime_tokens[&100],
        true,
        || Some(10),
        |_| panic!("an unrestored iconic window must not be restored again"),
        |_| false,
    );
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert!(state.window_last_maximized_at[&100] >= before_completion);
    assert_eq!(state.last_placed_layout_rects[&100], placed_rect);
    assert_eq!(
        state
            .injected_apply_placements_batches
            .lock()
            .unwrap()
            .len(),
        batches_before
    );
}

#[test]
fn test_self_maximize_outside_admission_settle_window_is_allowed() {
    let mut state = admit_unmaximized_for_late_maximize(Some(10));
    state.window_managed_at.insert(
        100,
        Instant::now() - crate::event_handler::SNAPBACK_SETTLE_AFTER_CREATE,
    );
    state.on_window_moved_or_resized_with_native_ops(
        100,
        false,
        || Some(10),
        |_| panic!("an established window must not restore"),
        |_| true,
    );
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert!(state.window_last_maximized_at.contains_key(&100));
}

#[test]
fn test_expired_late_maximize_eligibility_skips_early_native_queries() {
    let mut state = admit_unmaximized_for_late_maximize(Some(10));
    state.window_managed_at.insert(
        100,
        Instant::now() - crate::event_handler::SNAPBACK_SETTLE_AFTER_CREATE,
    );
    state.applying_layout = true;
    let queries = std::cell::Cell::new(0);
    state.on_window_moved_or_resized_with_native_ops(
        100,
        false,
        || panic!("expired eligibility must not sample input"),
        |_| panic!("expired eligibility must not restore"),
        |_| {
            queries.set(queries.get() + 1);
            false
        },
    );
    assert_eq!(
        queries.get(),
        1,
        "only the existing suppression query should run"
    );
    assert!(!state
        .post_admission_maximize_restore_eligible
        .contains_key(&100));
    assert!(!state.pending_maximized_admission_restores.contains(&100));
    assert!(state.deferred_moved_or_resized.contains(&100));
}

#[test]
fn test_single_late_maximize_during_admission_suppression_restores() {
    for (applying_layout, display_change_pending) in [(false, false), (true, false), (false, true)]
    {
        let mut state = admit_unmaximized_for_late_maximize(Some(10));
        state
            .moved_or_resized_suppression
            .insert(100, Instant::now() + Duration::from_millis(250));
        state.applying_layout = applying_layout;
        state.display_change_pending = display_change_pending;
        let queued = std::cell::Cell::new(0);
        let maximize_queries = std::cell::Cell::new(0);
        state.on_window_moved_or_resized_with_native_ops(
            100,
            false,
            || Some(10),
            |_| {
                queued.set(queued.get() + 1);
                Ok(true)
            },
            |_| {
                maximize_queries.set(maximize_queries.get() + 1);
                true
            },
        );
        assert_eq!(queued.get(), 1);
        assert_eq!(maximize_queries.get(), 1);
        assert!(state.pending_maximized_admission_restores.contains(&100));
    }
}

#[test]
fn test_allowed_first_maximize_cannot_restore_on_later_notification() {
    for counts in [[Some(11), Some(11), Some(12)], [None, Some(10), Some(12)]] {
        let mut state = admit_unmaximized_for_late_maximize(Some(10));
        for press_count in counts {
            state.on_window_moved_or_resized_with_native_ops(
                100,
                false,
                || press_count,
                |_| panic!("an allowed maximize must never restore on a later notification"),
                |_| true,
            );
            assert!(!state.pending_maximized_admission_restores.contains(&100));
            assert!(state.window_last_maximized_at.contains_key(&100));
        }
    }
}

#[test]
fn test_late_maximize_queue_failure_or_missing_report_does_not_retry() {
    for queue_fails in [false, true] {
        let mut state = admit_unmaximized_for_late_maximize(Some(10));
        let queued = std::cell::Cell::new(0);
        for _ in 0..2 {
            state.on_window_moved_or_resized_with_native_ops(
                100,
                false,
                || Some(10),
                |_| {
                    queued.set(queued.get() + 1);
                    if queue_fails {
                        Err(leopardwm_platform_win32::Win32Error::WindowNotFound(100))
                    } else {
                        Ok(false)
                    }
                },
                |_| true,
            );
        }
        assert_eq!(queued.get(), 1);
        assert!(!state.pending_maximized_admission_restores.contains(&100));
        assert!(state.window_last_maximized_at.contains_key(&100));
    }
}

fn admit_pending_maximized() -> AppState {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state.reduce_motion = true;
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    assert_eq!(
        state.try_admit_window_at_with_native_ops(
            100,
            AdmissionKind::Automatic,
            None,
            |_| true,
            |_| Ok(true)
        ),
        AdmitOutcome::Admitted
    );
    state.layout_transition = None;
    state
}

#[test]
fn test_maximized_admission_results_ignore_stale_or_departed_lifetimes() {
    for departed in [false, true] {
        for still_maximized in [false, true] {
            let mut state = admit_pending_maximized();
            let token = state.managed_lifetime_tokens[&100];
            if departed {
                state.workspaces.get_mut(&1).unwrap()[0]
                    .remove_window(100)
                    .unwrap();
            } else {
                state.managed_lifetime_tokens.insert(100, token + 1);
            }
            let grace = state.window_last_maximized_at[&100];
            let placements = state.last_placed_layout_rects.clone();
            state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
                window_id: 100,
                managed_lifetime_token: token,
                still_maximized,
            });
            assert_eq!(state.window_last_maximized_at[&100], grace);
            assert!(state.pending_maximized_admission_restores.contains(&100));
            assert_eq!(state.last_placed_layout_rects, placements);
        }
    }
}

#[test]
fn test_maximized_admission_queue_failure_uses_current_native_state() {
    for still_maximized in [false, true] {
        let mut state = AppState::new_with_config(test_config(), test_monitors());
        state
            .injected_window_info
            .insert(100, make_test_window_info(100));
        let queries = std::cell::Cell::new(0);
        assert_eq!(
            state.try_admit_window_at_with_native_ops(
                100,
                AdmissionKind::Automatic,
                None,
                |_| {
                    let query = queries.get();
                    queries.set(query + 1);
                    query == 0 || still_maximized
                },
                |_| Err(leopardwm_platform_win32::Win32Error::WindowNotFound(100)),
            ),
            AdmitOutcome::Admitted
        );
        assert_eq!(queries.get(), 2);
        assert_eq!(
            state.window_last_maximized_at.contains_key(&100),
            still_maximized
        );
        assert!(!state.pending_maximized_admission_restores.contains(&100));
    }
}

#[test]
fn test_maximized_admission_without_report_keeps_grace_when_worker_restores_first() {
    let mut state = AppState::new_with_config(test_config(), test_monitors());
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    let queued = std::cell::Cell::new(false);
    assert_eq!(
        state.try_admit_window_at_with_native_ops(
            100,
            AdmissionKind::Automatic,
            None,
            |_| !queued.get(),
            |_| {
                queued.set(true);
                Ok(false)
            },
        ),
        AdmitOutcome::Admitted
    );
    assert!(queued.get());
    assert!(state.window_last_maximized_at.contains_key(&100));
    assert!(!state.pending_maximized_admission_restores.contains(&100));
}

#[test]
fn test_unfocused_maximized_admission_keeps_per_app_column_width() {
    let mut config = test_config();
    config.behavior.focus_new_windows = false;
    config.window_rules.push(config::WindowRule {
        match_class: Some("TestWindowClass".into()),
        column_width: Some(0.35),
        ..Default::default()
    });
    let mut state = AppState::new_with_config(config, test_monitors());
    state
        .focused_workspace_mut()
        .unwrap()
        .insert_window(200, None)
        .unwrap();
    state
        .injected_window_info
        .insert(100, make_test_window_info(100));
    let width = (0.35 * f64::from(state.viewport_width_for(1))).round() as i32;
    assert_eq!(
        state.try_admit_window_at_with_native_ops(
            100,
            AdmissionKind::Automatic,
            None,
            |_| true,
            |_| Ok(true)
        ),
        AdmitOutcome::Admitted
    );
    state.paused = false;
    state.layout_transition = None;
    state.injected_apply_placements_behavior =
        Some(TestApplyPlacementsBehavior::SleepAndSucceed(Duration::ZERO));
    state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
        window_id: 100,
        managed_lifetime_token: state.managed_lifetime_tokens[&100],
        still_maximized: false,
    });
    let workspace = state.focused_workspace().unwrap();
    assert_eq!(workspace.focused_window(), Some(200));
    assert_eq!(
        workspace
            .columns()
            .iter()
            .find(|column| column.contains(100))
            .unwrap()
            .width(),
        width
    );
    assert_eq!(state.last_placed_layout_rects[&100].width, width);
}

#[test]
fn test_pending_maximized_admission_outlasts_grace_without_user_maximize_cleanup() {
    let mut state = admit_pending_maximized();
    let old = Instant::now() - Duration::from_secs(10);
    state.window_managed_at.insert(100, old);
    state.window_last_maximized_at.insert(100, old);
    state.ghost_sources_pending_safe_landing.insert(100);
    let placements = state
        .focused_workspace()
        .unwrap()
        .compute_placements_animated(state.layout_viewport(1));
    state.handle_maximized_placement_skips(&[100]);
    assert!(state.ghost_sources_pending_safe_landing.contains(&100));
    assert!(state
        .filter_physical_placements_observed(placements.clone(), &HashSet::new())
        .is_empty());
    state.handle_window_event(WindowEvent::MovedOrResized(100));
    assert!(state.ghost_sources_pending_safe_landing.contains(&100));
    state.handle_window_event(WindowEvent::MaximizedAdmissionRestored {
        window_id: 100,
        managed_lifetime_token: state.managed_lifetime_tokens[&100],
        still_maximized: false,
    });
    assert_eq!(
        state
            .filter_physical_placements_observed(placements, &HashSet::new())
            .len(),
        1
    );
}
