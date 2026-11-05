//! Dimensions in device-independent pixels. Painting, hit testing, and scroll
//! clipping share the same bounds.

pub(super) const CONTENT_X: i32 = 108;
pub(super) const CONTENT_RIGHT: i32 = 20;
pub(super) const VIEW_TOP: i32 = 96;
pub(super) const FOOTER_HEIGHT: i32 = 48;
pub(super) const GROUP_GAP: i32 = 16;
pub(super) const CONTROL_HEIGHT: i32 = 34;

pub(super) struct RowColumns {
    pub label_x: i32,
    pub label_width: i32,
    pub control_x: i32,
    pub control_width: i32,
    pub stacked: bool,
}

impl RowColumns {
    pub fn new(x: i32, width: i32, toggle: bool) -> Self {
        let label_x = x + 48;
        let right = x + width - 14;
        let control_width = if toggle { 46 } else { (width * 38 / 100).clamp(128, 248) };
        let control_x = right - control_width;
        let label_width = control_x - label_x - 14;
        if label_width < 124 {
            Self {
                label_x,
                label_width: (right - label_x).max(1),
                control_x: label_x,
                control_width: (right - label_x).max(1),
                stacked: true,
            }
        } else {
            Self { label_x, label_width, control_x, control_width, stacked: false }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_cards_stack_at_narrow_width_without_reducing_readable_text() {
        let narrow = RowColumns::new(260, 310, false);
        assert!(narrow.stacked);
        assert_eq!(narrow.label_x, narrow.control_x);
        assert!(narrow.label_width > 200);
        let wide = RowColumns::new(108, 470, false);
        assert!(!wide.stacked);
        assert!(wide.label_x + wide.label_width < wide.control_x);
        assert_eq!(wide.control_x + wide.control_width, 108 + 470 - 14);
    }
}
