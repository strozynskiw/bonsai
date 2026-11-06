//! Permission rules and matching facade.

mod command_approval;
mod implementation;

pub(crate) use command_approval::display_permission_pattern;
pub(crate) use implementation::*;
