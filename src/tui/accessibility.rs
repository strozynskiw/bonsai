/// Accessibility behavior selected for one interactive TUI launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TuiAccessibility {
    pub(crate) reduced_motion: bool,
    pub(crate) screen_reader: bool,
}

impl TuiAccessibility {
    /// Build a launch configuration. Screen-reader mode always disables motion.
    pub(crate) fn new(reduced_motion: bool, screen_reader: bool) -> Self {
        Self {
            reduced_motion: reduced_motion || screen_reader,
            screen_reader,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_reader_mode_implies_reduced_motion() {
        assert_eq!(
            TuiAccessibility::new(false, true),
            TuiAccessibility {
                reduced_motion: true,
                screen_reader: true,
            }
        );
    }
}
