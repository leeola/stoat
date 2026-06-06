//! Surface-elevation styling helpers. No surface routes through these
//! yet; the modal/picker, LSP popup, and toast chrome upgrades consume
//! them.
#![allow(dead_code)]

use crate::theme::ActiveTheme;
use gpui::{hsla, point, px, App, BoxShadow, Styled};

/// Extends [`gpui::Styled`] with stoat's elevated-surface treatment.
///
/// Each helper applies, in one call, the chrome a floating surface
/// needs to read as a layered surface rather than a flat rectangle: the
/// elevated-surface background, rounded corners, a 1px deemphasized
/// border ([`ThemeColors::border_variant`]), and a layered drop shadow
/// whose depth grows with the elevation level.
///
/// [`ThemeColors::border_variant`]: crate::theme::ThemeColors::border_variant
pub(crate) trait StyledExt: Styled + Sized {
    /// Non-modal elevated surface: floating panels, popups, and
    /// notifications that sit above the workspace but below modals.
    /// Applies a two-layer drop shadow.
    fn elevation_2(self, cx: &App) -> Self {
        elevated(self, cx, elevation_2_shadow())
    }

    /// Modal surface: dialogs and pickers that sit above the wash layer,
    /// the highest elevation a default-state surface renders at. Applies
    /// a four-layer drop shadow.
    fn elevation_3(self, cx: &App) -> Self {
        elevated(self, cx, elevation_3_shadow())
    }
}

impl<E: Styled> StyledExt for E {}

fn elevated<E: Styled>(this: E, cx: &App, shadow: Vec<BoxShadow>) -> E {
    let theme = cx.theme();
    this.bg(theme.elevated_surface)
        .rounded_lg()
        .border_1()
        .border_color(theme.border_variant)
        .shadow(shadow)
}

fn elevation_2_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: hsla(0., 0., 0., 0.12),
            offset: point(px(0.), px(2.)),
            blur_radius: px(3.),
            spread_radius: px(0.),
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.06),
            offset: point(px(0.), px(1.)),
            blur_radius: px(0.),
            spread_radius: px(0.),
        },
    ]
}

fn elevation_3_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: hsla(0., 0., 0., 0.12),
            offset: point(px(0.), px(2.)),
            blur_radius: px(3.),
            spread_radius: px(0.),
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.08),
            offset: point(px(0.), px(3.)),
            blur_radius: px(6.),
            spread_radius: px(0.),
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.04),
            offset: point(px(0.), px(6.)),
            blur_radius: px(12.),
            spread_radius: px(0.),
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.12),
            offset: point(px(0.), px(1.)),
            blur_radius: px(0.),
            spread_radius: px(0.),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{elevation_2_shadow, elevation_3_shadow, StyledExt};
    use crate::theme::ActiveTheme;
    use gpui::{div, Styled, TestAppContext};

    #[test]
    fn elevation_2_applies_border_variant_and_its_shadow_spec() {
        let cx = TestAppContext::single();
        cx.update(|cx| {
            let mut el = div().elevation_2(cx);
            let style = el.style();
            assert_eq!(style.border_color, Some(cx.theme().border_variant));
            assert_eq!(style.box_shadow, Some(elevation_2_shadow()));
        });
    }

    #[test]
    fn elevation_3_applies_border_variant_and_its_shadow_spec() {
        let cx = TestAppContext::single();
        cx.update(|cx| {
            let mut el = div().elevation_3(cx);
            let style = el.style();
            assert_eq!(style.border_color, Some(cx.theme().border_variant));
            assert_eq!(style.box_shadow, Some(elevation_3_shadow()));
        });
    }
}
