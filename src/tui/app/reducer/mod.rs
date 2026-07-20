use crate::tui::event::AppAction;

use super::AppState;

mod context_view;
mod core;
mod execution_group;
mod input;
mod modal;
mod pickers;
pub(crate) mod scroll;
mod transcript;

pub(super) enum ActionResult {
    Handled,
    Unhandled(Box<AppAction>),
}

impl ActionResult {
    pub(super) fn unhandled(action: AppAction) -> Self {
        Self::Unhandled(Box::new(action))
    }

    fn into_unhandled(self) -> Option<AppAction> {
        match self {
            Self::Handled => None,
            Self::Unhandled(action) => Some(*action),
        }
    }
}

macro_rules! try_handle {
    ($handler:ident, $app:expr, $action:expr) => {
        match $handler::handle($app, $action).into_unhandled() {
            Some(action) => action,
            None => return,
        }
    };
}

pub(super) fn reduce(app: &mut AppState, action: AppAction) {
    let action = try_handle!(input, app, action);
    let action = try_handle!(scroll, app, action);
    let action = try_handle!(transcript, app, action);
    let action = try_handle!(execution_group, app, action);
    let action = try_handle!(context_view, app, action);
    let action = try_handle!(pickers, app, action);
    let action = try_handle!(modal, app, action);

    if let Some(action) = core::handle(app, action).into_unhandled() {
        unreachable!("unhandled reducer action: {action:?}");
    }
}
