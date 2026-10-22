//! Live scroll resolution (HID++ 0x2121) for apps that ignore high-resolution wheel events.
//!
//! This is the mouse's live wheel mode, not an onboard profile. The kernel's
//! `hid-logitech-hidpp` turns high resolution back on whenever the mouse connects,
//! so a value written once does not stick. The daemon re-reads the mode while a
//! listed app is focused and writes only when it differs.

use hidpp::feature::hires_wheel::WheelResolution;

/// Scroll resolution as Omalogi uses it. HID++ has the same two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollResolution {
    /// One notch is one wheel click. Source 2 games need this.
    Low,
    /// Several events per notch. The kernel enables this on connect.
    High,
}

impl ScrollResolution {
    pub(crate) fn from_wheel(resolution: WheelResolution) -> Option<Self> {
        match resolution {
            WheelResolution::Low => Some(Self::Low),
            WheelResolution::High => Some(Self::High),
            _ => None,
        }
    }

    pub(crate) fn to_wheel(self) -> WheelResolution {
        match self {
            Self::Low => WheelResolution::Low,
            Self::High => WheelResolution::High,
        }
    }
}

/// What the daemon should do to the wheel, and what to put back later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPlan {
    /// Resolution to write. `None` when the wheel is already right.
    pub write: Option<ScrollResolution>,
    /// Resolution this daemon changed away from.
    ///
    /// Cleared once that resolution is current again, so a wheel that was already
    /// low is left alone when the app loses focus.
    pub restore: Option<ScrollResolution>,
}

/// Decides the next live wheel write.
///
/// `app_wants_low` is true while the focused window is listed in `discrete_scroll`.
/// `restore` is set only after this daemon itself switched the wheel to low.
#[must_use]
pub fn plan_scroll(
    app_wants_low: bool,
    current: ScrollResolution,
    restore: Option<ScrollResolution>,
) -> ScrollPlan {
    if app_wants_low {
        let restore = restore.or(match current {
            ScrollResolution::Low => None,
            other => Some(other),
        });
        return ScrollPlan {
            write: (current != ScrollResolution::Low).then_some(ScrollResolution::Low),
            restore,
        };
    }
    match restore {
        Some(saved) if current != saved => ScrollPlan {
            write: Some(saved),
            restore: None,
        },
        _ => ScrollPlan {
            write: None,
            restore: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(
        matched: bool,
        current: ScrollResolution,
        restore: Option<ScrollResolution>,
    ) -> ScrollPlan {
        plan_scroll(matched, current, restore)
    }

    #[test]
    fn a_matched_app_forces_low_and_remembers_high() {
        assert_eq!(
            plan(true, ScrollResolution::High, None),
            ScrollPlan {
                write: Some(ScrollResolution::Low),
                restore: Some(ScrollResolution::High),
            }
        );
    }

    #[test]
    fn a_wheel_already_low_is_not_claimed() {
        assert_eq!(
            plan(true, ScrollResolution::Low, None),
            ScrollPlan {
                write: None,
                restore: None,
            }
        );
    }

    #[test]
    fn staying_in_the_app_rewrites_low_after_the_kernel_turns_high_back_on() {
        assert_eq!(
            plan(true, ScrollResolution::High, Some(ScrollResolution::High)),
            ScrollPlan {
                write: Some(ScrollResolution::Low),
                restore: Some(ScrollResolution::High),
            }
        );
        assert_eq!(
            plan(true, ScrollResolution::Low, Some(ScrollResolution::High)),
            ScrollPlan {
                write: None,
                restore: Some(ScrollResolution::High),
            }
        );
    }

    #[test]
    fn leaving_the_app_restores_the_resolution_this_daemon_replaced() {
        assert_eq!(
            plan(false, ScrollResolution::Low, Some(ScrollResolution::High)),
            ScrollPlan {
                write: Some(ScrollResolution::High),
                restore: None,
            }
        );
    }

    #[test]
    fn leaving_does_nothing_when_the_wheel_was_not_changed_here() {
        assert_eq!(
            plan(false, ScrollResolution::High, None),
            ScrollPlan {
                write: None,
                restore: None,
            }
        );
        assert_eq!(
            plan(false, ScrollResolution::Low, None),
            ScrollPlan {
                write: None,
                restore: None,
            }
        );
    }

    #[test]
    fn an_already_restored_wheel_drops_the_saved_resolution() {
        assert_eq!(
            plan(false, ScrollResolution::High, Some(ScrollResolution::High)),
            ScrollPlan {
                write: None,
                restore: None,
            }
        );
    }
}
