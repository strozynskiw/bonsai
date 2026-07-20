//! Shared, cheaply-cloneable holder for the session's [`ApprovalLevel`] — the
//! single autonomy knob read by every tool. It drives both prompt suppression
//! (the bash gate) and the guardrails (project confinement, read-before-write).
//!
//! The type is still named `YoloMode` for now: it was historically a yes/no
//! "yolo" bool, and many call sites read `is_enabled()` to mean "all guardrails
//! off". That predicate is preserved (`is_enabled()` == the `Yolo` level), so
//! confinement/read-before-write sites stay correct without edits while the
//! richer level drives the rest. (Renaming the type to `ApprovalMode` is a
//! cosmetic follow-up.)

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::tool::ApprovalLevel;

#[derive(Clone, Debug)]
pub(crate) struct YoloMode {
    level: Arc<AtomicU8>,
}

impl YoloMode {
    pub(crate) fn new() -> Self {
        Self::with_level(ApprovalLevel::Ask)
    }

    pub(crate) fn with_level(level: ApprovalLevel) -> Self {
        Self {
            level: Arc::new(AtomicU8::new(level.as_u8())),
        }
    }

    pub(crate) fn level(&self) -> ApprovalLevel {
        ApprovalLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    pub(crate) fn set_level(&self, level: ApprovalLevel) {
        self.level.store(level.as_u8(), Ordering::Relaxed);
    }

    /// True only at the `Yolo` level — i.e. all guardrails removed. Kept under
    /// the historical name so the confinement / read-before-write call sites
    /// read correctly unchanged.
    pub(crate) fn is_enabled(&self) -> bool {
        self.level().bypasses_all()
    }

    /// Test-only shim: `on` → `Yolo`, `off` → `Ask`. Production paths (`/yolo`,
    /// `/autonomy`, Alt+M) set the level directly via [`Self::set_level`].
    #[cfg(test)]
    pub(crate) fn set_enabled(&self, on: bool) {
        self.set_level(if on {
            ApprovalLevel::Yolo
        } else {
            ApprovalLevel::Ask
        });
    }
}

impl Default for YoloMode {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this cleanup fixed: `auto-accept` used to flip the same bypass
    /// flag as `yolo`, so it silently dropped project confinement. It must now
    /// stay confined — only `yolo` removes guardrails.
    #[test]
    fn only_yolo_bypasses_guardrails() {
        let mode = YoloMode::with_level(ApprovalLevel::AutoAccept);
        assert!(!mode.is_enabled(), "auto-accept must stay confined");
        assert!(mode.level().is_confined());
        assert!(mode.level().requires_read_before_write());

        mode.set_level(ApprovalLevel::Yolo);
        assert!(mode.is_enabled());
        assert!(!mode.level().is_confined());
    }

    #[test]
    fn set_enabled_shim_maps_to_yolo_and_ask() {
        let mode = YoloMode::new();
        assert_eq!(mode.level(), ApprovalLevel::Ask);
        mode.set_enabled(true);
        assert_eq!(mode.level(), ApprovalLevel::Yolo);
        mode.set_enabled(false);
        assert_eq!(mode.level(), ApprovalLevel::Ask);
    }
}
