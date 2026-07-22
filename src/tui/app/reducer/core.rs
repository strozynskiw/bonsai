use crate::agent::{ActivePersona, AgentMode, PersonaView};
use crate::tui::event::{AppAction, Focus, View};

use super::super::{AppState, TranscriptItem};
use super::ActionResult;

/// Default agent mode for a given view. Used by `SetView` to keep the persona in
/// sync when the view changes explicitly (digit shortcuts, `/start`, `/review`).
fn default_mode_for_view(view: View) -> AgentMode {
    match view {
        View::Agent => AgentMode::Coding,
        View::Plan => AgentMode::Planning,
    }
}

/// The binary `View` the active persona maps to, for the still-`View`-keyed
/// consumers (keymap canvas gates, accent). Only the canvas surface uses `Plan`.
fn view_for_active_persona(app: &AppState) -> View {
    match app.surface() {
        PersonaView::Canvas => View::Plan,
        _ => View::Agent,
    }
}

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::Tick => {
            app.tick = app.tick.wrapping_add(1);
            app.clear_expired_copy_notice();
            app.clear_expired_session_hint();
            app.clear_expired_session_toast();
        }
        AppAction::ReplantBonsai => {
            app.sidebar_bonsai.replant();
        }
        AppAction::ToggleMouseCapture => {
            // Flip the desired state; the run loop reconciles the real terminal
            // (Enable/DisableMouseCapture) to this flag after the reduce. The
            // persistent meta-line marker tracks it; this transient notice
            // confirms the switch and reminds how to reverse it.
            app.mouse_capture = !app.mouse_capture;
            let text = if app.mouse_capture {
                "Mouse capture on — in-app scroll & click restored"
            } else {
                "Mouse capture off — drag to select text; Ctrl+G or /select to re-enable"
            };
            app.set_copy_notice(text, crate::tui::app::CopyNoticeKind::Status);
        }
        AppAction::ToggleView => {
            // The switcher cycles the personas — built-in (coding → planning) then enabled
            // custom agents — against a live registry snapshot; `view` follows the
            // selected persona for the consumers still keyed on the binary view.
            let registry = crate::resource::agent::snapshot(&app.custom_agents);
            app.active_persona = crate::agent::next_persona(&app.active_persona, &registry);
            app.view = view_for_active_persona(app);
            if !matches!(app.view, View::Agent) {
                app.active_group_tool_selection = None;
            }
            app.normalize_hidden_focus();
            app.transcript_autoscroll = true;
        }
        AppAction::SetView(view) => {
            app.view = view;
            app.active_persona = ActivePersona::Builtin(default_mode_for_view(view));
            if !matches!(app.view, View::Agent) {
                app.active_group_tool_selection = None;
            }
            app.transcript_autoscroll = true;
            app.normalize_hidden_focus();
        }
        AppAction::ImplementPlan => {
            // Runtime effect: `tui::runtime_actions` owns the agent task and
            // composes the hand-off prompt from the plan canvas.
        }
        AppAction::SetShutdownNotice(notice) => {
            app.shutdown_notice = notice;
        }
        AppAction::SetTaskState(state) => {
            app.task_state = state;
            if matches!(state, crate::tui::event::TaskState::Running) {
                app.current_session_status = crate::storage::SessionStatus::Active;
                app.current_terminal_reason = None;
            }
        }
        AppAction::CommandOutput { kind, text } => {
            app.push_transcript_item(TranscriptItem::CommandOutput { kind, text });
            app.scroll_to_bottom_current();
        }
        AppAction::PeerMessage {
            source_message_id,
            delivery_receipt,
            session_id,
            outgoing,
            text,
        } => {
            if let Some(receipt) = delivery_receipt {
                app.pending_peer_delivery_receipts
                    .insert(receipt.message_id(), receipt);
            }
            // Inter-agent chat (peers P2): render the blue conversation
            // message in the transcript flow, both directions. A replayed
            // lease reuses the durable source id instead of duplicating it.
            let already_rendered = source_message_id.is_some_and(|message_id| {
                app.transcript.iter().any(|item| {
                    matches!(
                        item,
                        TranscriptItem::PeerMessage {
                            source_message_id: Some(existing),
                            outgoing: false,
                            ..
                        } if *existing == message_id
                    )
                })
            });
            if !already_rendered {
                app.push_transcript_item(TranscriptItem::PeerMessage {
                    source_message_id,
                    session_id,
                    outgoing,
                    text,
                });
            }
            app.scroll_to_bottom_current();
        }
        AppAction::PeerInboxChanged { count } => {
            app.pending_peer_inbox = count;
        }
        AppAction::SetApprovalLevel(level) => {
            app.approval_level = level;
        }
        AppAction::SetSmolMode(on) => {
            app.smol_mode = on;
        }
        AppAction::SetSerenityMode(on) => {
            app.serenity_mode = on;
            app.active_group_tool_selection = None;
            if !on {
                app.expanded_execution_groups.clear();
            }
        }
        AppAction::CycleApprovalLevel => {
            // Handled in `runtime_actions`, where the shared holder is reachable
            // without locking the agent mid-turn; it computes the next level and
            // mirrors it back via `SetApprovalLevel`.
        }
        #[cfg(test)]
        AppAction::ProviderModelChanged {
            provider,
            model,
            reasoning,
        } => {
            app.provider = provider;
            app.model = model;
            app.reasoning = reasoning;
        }
        AppAction::SetFocus(focus) => {
            if !matches!(focus, Focus::Transcript) {
                app.active_group_tool_selection = None;
            }
            if matches!(focus, Focus::Input) {
                app.transcript_selection = None;
                app.transcript_focus = None;
                app.scroll_transcript_focus_into_view = false;
            }
            app.focus = focus;
        }
        AppAction::Agent(event) => app.apply_event(event),
        AppAction::Runtime(event) => app.apply_runtime_event(event),
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}
