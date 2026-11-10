//! The goal loop as `app.rs` drives it: prompt, one turn, classify, repeat.
//!
//! These tests stay on the public surface — nothing here reaches into private
//! state — because the point is the contract the board's agent thread is built on.
//! Each one plays a scripted model, so "does a goal stop after three failures?"
//! and "does a reboot resume where it left off?" are answered without a key, a
//! network or a bench board.

use esp32_agent_core::goal::{
    classify, Goal, GoalStatus, Reply, DEFAULT_MAX_ITERATIONS, MAX_CONSECUTIVE_FAILURES, NOTE_CAP,
    NOTE_MAX_CHARS, PROMPT_MAX_CHARS,
};

/// What the agent thread does for one iteration: the goal offers a prompt, the
/// turn runs, the reply comes back.
fn step(goal: &mut Goal, reply: &str, now_ms: u64) -> GoalStatus {
    assert!(goal.next_prompt().is_some(), "an active goal must offer a prompt for this iteration");
    goal.observe(reply, now_ms)
}

/// A whole loop with a scripted model, returning every state the loop passed
/// through. `None` from the prompt ends it, exactly as `sched_tick` would.
fn run_loop(goal: &mut Goal, replies: &[&str]) -> Vec<GoalStatus> {
    let mut seen = Vec::new();
    for (i, reply) in replies.iter().enumerate() {
        if goal.next_prompt().is_none() {
            break;
        }
        seen.push(goal.observe(reply, 1_000 + i as u64 * 1_000));
    }
    seen
}

#[test]
fn goal_runs_the_iterations_itself_and_stops_at_the_cap() {
    let mut goal = Goal::new("get the greenhouse to 60% humidity", 4, 0).expect("goal");

    // The prompt is what makes this a loop rather than a chat: it moves with the
    // iteration number and carries what the previous turns learned.
    let first = goal.next_prompt().expect("prompt");
    assert!(first.contains("Iteration 1 of 4"), "{first}");
    assert!(first.contains("(none yet)"), "{first}");

    let states = run_loop(
        &mut goal,
        &["opened the vent a little", "ran the misting for 20 s", "measured 55%", "waited"],
    );
    assert_eq!(
        states,
        vec![GoalStatus::Running, GoalStatus::Running, GoalStatus::Running, GoalStatus::Exhausted]
    );
    assert_eq!(goal.iteration, 4, "four iterations, no more");
    assert_eq!(goal.notes.len(), 4, "every iteration left one note");
    assert!(goal.next_prompt().is_none(), "the loop is over");
    assert!(goal.stop_reason.contains('4'), "the cap is named: {}", goal.stop_reason);

    // The notes accumulated in the order the loop read them.
    let last = Goal::from_json(&goal.to_json()).expect("load").notes;
    assert_eq!(last[0], "opened the vent a little", "oldest first");
    assert_eq!(last[3], "waited", "newest last");
}

#[test]
fn goal_finishes_at_the_first_turn_that_reports_met() {
    let mut goal = Goal::new("make the led blink", DEFAULT_MAX_ITERATIONS, 0).expect("goal");

    assert_eq!(step(&mut goal, "wrote a blink loop", 1_000), GoalStatus::Running);
    assert_eq!(step(&mut goal, "flashed it, the led blinks\nGOAL MET", 2_000), GoalStatus::Met);

    assert_eq!(goal.iteration, 2, "the remaining budget is not spent");
    assert_eq!(goal.last_result, "flashed it, the led blinks");
    assert!(goal.next_prompt().is_none());
    assert!(!goal.status.is_active());
}

#[test]
fn goal_reports_blocked_instead_of_burning_the_budget() {
    let mut goal = Goal::new("read the missing sensor", 10, 0).expect("goal");
    assert_eq!(step(&mut goal, "no sensor on the bus", 1_000), GoalStatus::Running);
    assert_eq!(step(&mut goal, "GOAL BLOCKED - nothing is wired to i2c", 2_000), GoalStatus::Blocked);
    assert_eq!(goal.iteration, 2);
    assert!(goal.stop_reason.contains("nothing is wired"), "{}", goal.stop_reason);
    assert!(goal.next_prompt().is_none());
}

#[test]
fn goal_gives_up_after_the_consecutive_failure_limit() {
    let mut goal = Goal::new("upload the log", 50, 0).expect("goal");
    for i in 0..MAX_CONSECUTIVE_FAILURES {
        let status = goal.fail_iteration("chat completions timed out", 1_000 * u64::from(i + 1));
        let last = i + 1 == MAX_CONSECUTIVE_FAILURES;
        assert_eq!(status.is_active(), !last, "failure {} of the streak", i + 1);
    }
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert_eq!(goal.failures, MAX_CONSECUTIVE_FAILURES);
    assert!(
        goal.stop_reason.contains(&MAX_CONSECUTIVE_FAILURES.to_string()),
        "the reason names the count: {}",
        goal.stop_reason
    );
    assert_eq!(goal.iteration, MAX_CONSECUTIVE_FAILURES, "failures cost budget too");
    assert!(goal.next_prompt().is_none());
}

#[test]
fn a_successful_turn_keeps_a_broken_uplink_from_ending_the_goal() {
    let mut goal = Goal::new("wait for the sensor to settle", 10, 0).expect("goal");
    goal.fail_iteration("timeout", 1);
    goal.fail_iteration("timeout", 2);
    assert_eq!(goal.status, GoalStatus::Running, "two failures are survivable");
    assert_eq!(step(&mut goal, "read the value: 21.4", 3_000), GoalStatus::Running);
    assert_eq!(goal.failures, 0, "the streak is broken");

    // Two more after the success: still running, because the limit counts
    // *consecutive* failures.
    goal.fail_iteration("timeout", 4);
    goal.fail_iteration("timeout", 5);
    assert_eq!(goal.status, GoalStatus::Running);
    assert_eq!(goal.iteration, 5);
}

#[test]
fn goal_stopped_by_a_person_never_runs_again() {
    let mut goal = Goal::new("water every hour", 10, 0).expect("goal");
    step(&mut goal, "watered once", 1_000);

    goal.stop("operator stopped the bench", 2_000);
    assert_eq!(goal.status, GoalStatus::Stopped);
    assert!(goal.next_prompt().is_none(), "a stopped goal is not scheduled");

    // Even reloaded from NVS it stays stopped: nothing in this module resumes.
    let back = Goal::from_json(&goal.to_json()).expect("load");
    assert_eq!(back.status, GoalStatus::Stopped);
    assert!(back.next_prompt().is_none());
    assert_eq!(back.stop_reason, "operator stopped the bench");
}

#[test]
fn goal_survives_a_reboot_mid_loop() {
    let mut goal = Goal::new("collect three readings", 5, 0).expect("goal");
    step(&mut goal, "reading one: 20.1", 1_000);
    step(&mut goal, "reading two: 20.3", 2_000);

    // NVS holds text; the board writes it on every state change and reads it at boot.
    let stored = serde_json::to_string(&goal.to_json()).expect("serialise");
    let mut back = Goal::from_json(&serde_json::from_str(&stored).expect("parse")).expect("load");

    assert_eq!(back.status, GoalStatus::Running, "a running goal keeps running");
    assert_eq!(back.iteration, 2);
    assert_eq!(back.notes, goal.notes);
    assert_eq!(
        back.next_prompt(),
        goal.next_prompt(),
        "the prompt after a reboot is the one the board would have sent"
    );

    // And the loop picks up where it was.
    assert_eq!(step(&mut back, "reading three: 20.2\nGOAL MET", 3_000), GoalStatus::Met);
    assert_eq!(back.iteration, 3);
}

#[test]
fn goal_without_an_objective_is_refused() {
    assert!(Goal::new("", 10, 0).is_none());
    assert!(Goal::new("  \n\t ", 10, 0).is_none());
    assert!(Goal::new("turn on the fan", 10, 0).is_some());
}

#[test]
fn goal_notes_are_one_line_and_bounded() {
    let mut goal = Goal::new("describe the room", 100, 0).expect("goal");
    let reply = format!("line one {}\nline two\nline three", "z".repeat(NOTE_MAX_CHARS * 3));
    step(&mut goal, &reply, 1_000);

    assert_eq!(goal.notes.len(), 1);
    assert_eq!(goal.notes[0].chars().count(), NOTE_MAX_CHARS);
    assert!(!goal.notes[0].contains('\n'), "a note is one line");
    assert!(!goal.notes[0].contains("line two"));

    // A full note ring keeps the newest and drops the oldest.
    for i in 0..NOTE_CAP + 3 {
        step(&mut goal, &format!("step {i}"), 2_000 + i as u64);
    }
    assert_eq!(goal.notes.len(), NOTE_CAP);
    assert_eq!(*goal.notes.last().expect("a note"), format!("step {}", NOTE_CAP + 2));
}

#[test]
fn goal_prompt_stays_inside_its_budget_even_with_a_full_note_ring() {
    let mut goal = Goal::new("water the plants", 100, 0).expect("goal");
    for i in 0..NOTE_CAP {
        step(&mut goal, &format!("step {i} {}", "y".repeat(NOTE_MAX_CHARS)), 1_000 + i as u64);
    }
    let prompt = goal.next_prompt().expect("still running");
    assert!(
        prompt.chars().count() <= PROMPT_MAX_CHARS,
        "{} characters, budget {PROMPT_MAX_CHARS}",
        prompt.chars().count()
    );
    // What the model must be told every single iteration.
    for needle in ["water the plants", "Iteration", "GOAL MET", "GOAL BLOCKED", "one concrete step"] {
        assert!(prompt.contains(needle), "{needle} missing from: {prompt}");
    }
}

#[test]
fn classify_is_the_only_thing_that_decides_met_and_blocked() {
    // The loop calls this; the tests above would pass even if it were hard-wired.
    assert_eq!(classify("all done\nGOAL MET\n"), Reply::Met);
    assert_eq!(classify("goal blocked: no power"), Reply::Blocked);
    assert_eq!(classify("still working on it"), Reply::Continue);
}
