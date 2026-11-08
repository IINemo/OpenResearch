//! Goals with a deadline. When a chat pursuing one goes quiet with nothing in
//! flight that would wake it (a run, a result, a delegate, a question to the
//! user), orx starts its next turn toward the goal, until the agent reports it
//! met or blocked, the deadline passes, or orx's turns stop producing work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::delegation::pending_prompt;
use super::{
    ChatHost, OrxDisplay, OrxKind, OrxSource, TurnGuard, TurnState, TurnSubmission,
    ORX_MESSAGE_PREFIX,
};
use crate::error::Result;
use crate::store::{now_ms, GoalState, PursuedGoal, Store, StoredChatSession};

/// How long a chat stays quiet before orx starts a turn, so the user can speak first.
const QUIET: Duration = Duration::from_secs(2 * 60);

/// Turns in a row orx may start that launch no run or experiment and draw no
/// reply before it stops pursuing the goal.
const NUDGES_WITHOUT_PROGRESS: i64 = 3;

/// How the agent settles its goal without the orx MCP tools.
const CLI_GOAL: &str = r#"printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"goal","arguments":{"action":"met","evidence":"…"}}}' | orx mcp"#;

/// The goal block every turn carries; a goal that was met or ran out carries none.
pub(super) fn turn_context(session: &StoredChatSession, now: i64) -> Option<String> {
    let goal = session.goal.as_deref()?;
    let Some(pursuit) = session
        .goal_pursuit
        .as_ref()
        .filter(|pursuit| pursuit.state == GoalState::Active)
    else {
        return match session.goal_pursuit.as_ref().map(|pursuit| pursuit.state) {
            None | Some(GoalState::Paused) => Some(format!(
                "<orx-goal>\nKeep working toward this goal until it is met, across every turn:\n\n{goal}\n</orx-goal>"
            )),
            Some(_) => None,
        };
    };
    Some(format!(
        "<orx-goal deadline=\"{deadline}\" remaining=\"{remaining}\">\n\
         Keep working toward this goal on your own until it is met or the deadline passes:\n\n\
         {goal}\n\n\
         The user may be away until then. When you end a turn with nothing in flight (no run, \
         result or delegate still to come, no question to the user), orx starts your next turn \
         toward this goal, so end turns after launching work rather than waiting on it. \
         Disappointing results, several failed or regressed runs in a row, or a node at its \
         repair cap mean change direction (a new round, an idea from the literature, delegates \
         exploring in parallel), not stop and ask; this overrides any skill guidance to stop or \
         check in. Ask the user only for what you cannot do without them, such as credentials \
         or access.\n\n\
         When the goal is met, call the orx `goal` tool with action `met` and the evidence, \
         citing the runs that show it. If you cannot go on without the user, call it with \
         action `blocked` and what you need: orx then stops starting turns until they return. \
         Without the orx MCP tools, run `{CLI_GOAL}`.\n\
         </orx-goal>",
        deadline = crate::telemetry::iso8601_utc(pursuit.deadline_at),
        remaining = remaining(pursuit.deadline_at - now),
    ))
}

/// What the `goal` tool's `status` reports.
pub fn status(session: &StoredChatSession) -> String {
    let Some(goal) = session.goal.as_deref() else {
        return "This chat has no goal.".into();
    };
    let Some(pursuit) = &session.goal_pursuit else {
        return format!(
            "Goal: {goal}\n\nIt has no deadline: orx reminds you of it every turn but starts no turns toward it."
        );
    };
    let deadline = crate::telemetry::iso8601_utc(pursuit.deadline_at);
    let state = match pursuit.state {
        GoalState::Active => format!(
            "Pursuing it until {deadline}, {} from now.",
            remaining(pursuit.deadline_at - now_ms())
        ),
        GoalState::Met => "Reported met.".into(),
        GoalState::Expired => format!("The deadline passed at {deadline}."),
        GoalState::Paused => {
            "Paused: orx starts no turns toward it until the user resumes it.".into()
        }
    };
    let note = pursuit
        .note
        .as_deref()
        .map(|note| format!("\n\nNote: {note}"))
        .unwrap_or_default();
    format!("Goal: {goal}\n\n{state}{note}")
}

/// A coarse span such as `6d 23h`, `5h 12m` or `12m`.
fn remaining(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

/// What the store cannot say about a chat in one query.
struct Facts {
    /// The latest turn's state and when it last changed; `None` before the first.
    last_turn: Option<(String, i64)>,
    /// The chat waits on the user: a question, or a step to approve.
    waiting_on_user: bool,
    /// A chat it delegated to or subscribes to is working or owes it news.
    chats_outstanding: bool,
}

impl Facts {
    fn read(store: &Store, session_id: &str) -> Result<Self> {
        Ok(Self {
            last_turn: store
                .latest_chat_turn(session_id)?
                .map(|turn| (turn.state, turn.updated_at)),
            waiting_on_user: pending_prompt(store, session_id)?.is_some(),
            chats_outstanding: store
                .list_chat_subscriptions(Some(session_id))?
                .iter()
                .any(|sub| !sub.paused && sub.outstanding),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Wait,
    /// Start a turn toward the goal; `nudges` counts it among those without progress.
    Nudge {
        nudges: i64,
    },
    /// Orx's turns stopped producing work.
    Pause,
    /// The deadline passed: one last turn to report where the goal stands.
    WrapUp,
    /// The deadline passed while the chat needs the user; end without a turn.
    Expire,
}

fn next_step(goal: &PursuedGoal, facts: &Facts, now: i64, quiet: i64) -> Step {
    let (last_state, quiet_since) = match &facts.last_turn {
        None => (None, goal.nudged_at),
        Some((state, ended_at)) => (Some(state.as_str()), goal.nudged_at.max(*ended_at)),
    };
    if !matches!(
        last_state,
        None | Some("completed" | "failed" | "interrupted")
    ) {
        return Step::Wait;
    }
    // A failed or stopped turn is the user's to resolve; orx does not talk over it.
    let resolved = matches!(last_state, None | Some("completed")) && !facts.waiting_on_user;
    if now >= goal.deadline_at {
        return if resolved { Step::WrapUp } else { Step::Expire };
    }
    if !resolved || goal.runs_active || goal.notices_pending || facts.chats_outstanding {
        return Step::Wait;
    }
    if now - quiet_since < quiet {
        return Step::Wait;
    }
    if goal.progressed {
        Step::Nudge { nudges: 1 }
    } else if goal.nudges >= NUDGES_WITHOUT_PROGRESS {
        Step::Pause
    } else {
        Step::Nudge {
            nudges: goal.nudges + 1,
        }
    }
}

fn nudge_text(goal: &PursuedGoal, first: bool, now: i64) -> String {
    let left = remaining(goal.deadline_at - now);
    if first {
        format!(
            "{ORX_MESSAGE_PREFIX}Start working toward the goal. {left} remain until its deadline."
        )
    } else {
        format!(
            "{ORX_MESSAGE_PREFIX}You ended your last turn with nothing in flight, and the goal is not \
             reported met. {left} remain until its deadline. Re-read the experiment tree and the \
             latest results, then launch the next round. If the goal is met or you cannot go on \
             without the user, call the `goal` tool instead."
        )
    }
}

const WRAP_UP: &str = "The goal's deadline has passed. Launch nothing new. Report where the goal \
     stands: the best result against the baseline with its runs as evidence, what was tried, and \
     what remains. Write it up as a project artifact (load `orx-reports`). Runs still in flight \
     report here when they end.";

/// Start each quiet chat's next turn toward its goal, and end pursuits whose
/// deadline passed or whose turns stopped producing work.
pub(super) async fn pursue_goals(
    chat: &Arc<ChatHost>,
    store: Store,
    data_dir_move_in_progress: Option<&AtomicBool>,
) -> Result<()> {
    let moving = || data_dir_move_in_progress.is_some_and(|flag| flag.load(Ordering::SeqCst));
    let quiet = crate::config::paced(QUIET).as_millis() as i64;
    for goal in store.pursued_goals()? {
        if moving() {
            return Ok(());
        }
        let id = goal.session_id.as_str();
        if chat.is_busy(id).await {
            continue;
        }
        let facts = Facts::read(&store, id)?;
        let now = now_ms();
        let (text, nudges) = match next_step(&goal, &facts, now, quiet) {
            Step::Wait => continue,
            Step::Pause => {
                chat.emit_session(settle(&store, id, GoalState::Paused)?)
                    .await;
                continue;
            }
            Step::Expire => {
                chat.emit_session(settle(&store, id, GoalState::Expired)?)
                    .await;
                continue;
            }
            Step::WrapUp => (format!("{ORX_MESSAGE_PREFIX}{WRAP_UP}"), None),
            Step::Nudge { nudges } => (
                nudge_text(&goal, facts.last_turn.is_none(), now),
                Some(nudges),
            ),
        };
        let Some(guard) =
            TurnGuard::claim_idle(chat, id, TurnState::Reserved { turn_id: None }).await
        else {
            continue;
        };
        let shown = OrxDisplay {
            text: text
                .strip_prefix(ORX_MESSAGE_PREFIX)
                .unwrap_or(&text)
                .to_string(),
            source: OrxSource {
                kind: OrxKind::Message,
                from: None,
                status: None,
            },
        };
        match chat.send_orx_messages(id, vec![(text, shown)], guard).await {
            Ok(TurnSubmission::Started(_)) => match nudges {
                Some(nudges) => store.record_chat_session_goal_nudge(id, nudges)?,
                None => {
                    chat.emit_session(settle(&store, id, GoalState::Expired)?)
                        .await;
                }
            },
            Ok(_) => {}
            Err(error) => eprintln!("orx up: goal for {id}: {error}"),
        }
    }
    Ok(())
}

/// End a pursuit orx stopped itself, returning the chat to broadcast; the
/// dashboard words the reason from the state.
fn settle(store: &Store, id: &str, state: GoalState) -> Result<Option<StoredChatSession>> {
    if !store.settle_chat_session_goal(id, state, None)? {
        return Ok(None);
    }
    store.get_chat_session(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET_MS: i64 = 120_000;
    const NOW: i64 = 10_000_000;

    fn goal() -> PursuedGoal {
        PursuedGoal {
            session_id: "chat".into(),
            deadline_at: NOW + 7 * 86_400_000,
            nudges: 0,
            nudged_at: NOW - 3_600_000,
            runs_active: false,
            notices_pending: false,
            progressed: false,
        }
    }

    fn ended(state: &str, ago: i64) -> Facts {
        Facts {
            last_turn: Some((state.into(), NOW - ago)),
            waiting_on_user: false,
            chats_outstanding: false,
        }
    }

    #[test]
    fn a_quiet_chat_with_nothing_in_flight_is_nudged() {
        let facts = ended("completed", QUIET_MS);
        assert_eq!(
            next_step(&goal(), &facts, NOW, QUIET_MS),
            Step::Nudge { nudges: 1 }
        );
    }

    #[test]
    fn a_new_goal_starts_work_once_the_user_had_time_to_speak() {
        let facts = Facts {
            last_turn: None,
            waiting_on_user: false,
            chats_outstanding: false,
        };
        let fresh = PursuedGoal {
            nudged_at: NOW - 1_000,
            ..goal()
        };
        assert_eq!(next_step(&fresh, &facts, NOW, QUIET_MS), Step::Wait);
        assert_eq!(
            next_step(&goal(), &facts, NOW, QUIET_MS),
            Step::Nudge { nudges: 1 }
        );
    }

    #[test]
    fn anything_that_will_wake_the_chat_holds_the_nudge() {
        let quiet = ended("completed", QUIET_MS);
        let running = PursuedGoal {
            runs_active: true,
            ..goal()
        };
        let notified = PursuedGoal {
            notices_pending: true,
            ..goal()
        };
        assert_eq!(next_step(&running, &quiet, NOW, QUIET_MS), Step::Wait);
        assert_eq!(next_step(&notified, &quiet, NOW, QUIET_MS), Step::Wait);
        let delegating = Facts {
            chats_outstanding: true,
            ..ended("completed", QUIET_MS)
        };
        assert_eq!(next_step(&goal(), &delegating, NOW, QUIET_MS), Step::Wait);
        let asking = Facts {
            waiting_on_user: true,
            ..ended("completed", QUIET_MS)
        };
        assert_eq!(next_step(&goal(), &asking, NOW, QUIET_MS), Step::Wait);
        assert_eq!(
            next_step(&goal(), &ended("completed", 1_000), NOW, QUIET_MS),
            Step::Wait
        );
        assert_eq!(
            next_step(&goal(), &ended("running", QUIET_MS), NOW, QUIET_MS),
            Step::Wait
        );
    }

    #[test]
    fn a_failed_or_stopped_turn_is_left_to_the_user() {
        for state in ["failed", "interrupted"] {
            assert_eq!(
                next_step(&goal(), &ended(state, QUIET_MS), NOW, QUIET_MS),
                Step::Wait
            );
        }
    }

    #[test]
    fn nudges_without_progress_pause_the_goal_and_progress_resets_them() {
        let facts = ended("completed", QUIET_MS);
        let stalled = PursuedGoal {
            nudges: NUDGES_WITHOUT_PROGRESS - 1,
            ..goal()
        };
        assert_eq!(
            next_step(&stalled, &facts, NOW, QUIET_MS),
            Step::Nudge {
                nudges: NUDGES_WITHOUT_PROGRESS
            }
        );
        let spent = PursuedGoal {
            nudges: NUDGES_WITHOUT_PROGRESS,
            ..goal()
        };
        assert_eq!(next_step(&spent, &facts, NOW, QUIET_MS), Step::Pause);
        let progressed = PursuedGoal {
            progressed: true,
            ..spent
        };
        assert_eq!(
            next_step(&progressed, &facts, NOW, QUIET_MS),
            Step::Nudge { nudges: 1 }
        );
    }

    #[test]
    fn the_deadline_wraps_up_even_with_runs_in_flight() {
        let late = PursuedGoal {
            deadline_at: NOW,
            runs_active: true,
            ..goal()
        };
        assert_eq!(
            next_step(&late, &ended("completed", 1_000), NOW, QUIET_MS),
            Step::WrapUp
        );
        assert_eq!(
            next_step(&late, &ended("running", 1_000), NOW, QUIET_MS),
            Step::Wait
        );
        for state in ["failed", "interrupted"] {
            assert_eq!(
                next_step(&late, &ended(state, 1_000), NOW, QUIET_MS),
                Step::Expire
            );
        }
        let asking = Facts {
            waiting_on_user: true,
            ..ended("completed", 1_000)
        };
        assert_eq!(next_step(&late, &asking, NOW, QUIET_MS), Step::Expire);
    }

    fn session(pursuit: Option<GoalState>) -> StoredChatSession {
        StoredChatSession {
            id: "chat".into(),
            project_id: "project".into(),
            harness: "claude-code".into(),
            native_session_id: None,
            title: None,
            title_source: None,
            model: None,
            service_tier: None,
            permission_mode: None,
            plan_mode: false,
            plan_reset_pending: false,
            reasoning_level: None,
            archived: false,
            context_usage_json: None,
            bootstrap_context: None,
            goal: Some("halve latency".into()),
            goal_pursuit: pursuit.map(|state| crate::store::GoalPursuit {
                deadline_at: NOW + 2 * 3_600_000,
                state,
                note: None,
            }),
            autonomy: None,
            active_leaf_id: None,
            side_parent_session_id: None,
            side_parent_leaf_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_pursued_goal_tells_the_agent_its_deadline_and_how_to_settle_it() {
        let block = turn_context(&session(Some(GoalState::Active)), NOW).unwrap();
        assert!(block.starts_with("<orx-goal deadline=\""));
        assert!(block.contains("remaining=\"2h 0m\""));
        assert!(block.contains("halve latency"));
        assert!(block.contains("`goal` tool"));
        assert!(block.contains("| orx mcp"));
    }

    #[test]
    fn a_goal_without_a_running_pursuit_is_a_plain_reminder_until_it_ends() {
        for state in [None, Some(GoalState::Paused)] {
            let block = turn_context(&session(state), NOW).unwrap();
            assert!(block.starts_with("<orx-goal>\n"));
            assert!(block.contains("halve latency"));
        }
        for state in [GoalState::Met, GoalState::Expired] {
            assert_eq!(turn_context(&session(Some(state)), NOW), None);
        }
    }

    #[test]
    fn remaining_time_reads_coarsely() {
        assert_eq!(remaining(7 * 86_400_000 - 60_000), "6d 23h");
        assert_eq!(remaining(5 * 3_600_000 + 12 * 60_000), "5h 12m");
        assert_eq!(remaining(12 * 60_000 + 59_000), "12m");
        assert_eq!(remaining(-5), "0m");
    }
}
