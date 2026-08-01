//! Applies the Unreal Editor inspired desktop palette and compact control spacing.

use std::collections::BTreeMap;

use eframe::egui::style::WidgetVisuals;
use eframe::egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2, Visuals,
};

pub(crate) const TITLE: Color32 = Color32::from_rgb(0x15, 0x15, 0x15);
pub(crate) const RECESSED: Color32 = Color32::from_rgb(0x0f, 0x0f, 0x0f);
pub(crate) const PANEL: Color32 = Color32::from_rgb(0x24, 0x24, 0x24);
pub(crate) const HEADER: Color32 = Color32::from_rgb(0x2f, 0x2f, 0x2f);
pub(crate) const SECONDARY: Color32 = Color32::from_rgb(0x38, 0x38, 0x38);
pub(crate) const OUTLINE: Color32 = Color32::from_rgb(0x4c, 0x4c, 0x4c);
pub(crate) const HOVER: Color32 = Color32::from_rgb(0x57, 0x57, 0x57);
pub(crate) const FOREGROUND: Color32 = Color32::from_rgb(0xc0, 0xc0, 0xc0);
pub(crate) const FOREGROUND_HEADER: Color32 = Color32::from_rgb(0xc8, 0xc8, 0xc8);
pub(crate) const PRIMARY: Color32 = Color32::from_rgb(0x00, 0x70, 0xe0);
pub(crate) const PRIMARY_HOVER: Color32 = Color32::from_rgb(0x0e, 0x86, 0xff);
pub(crate) const SELECTED: Color32 = Color32::from_rgb(0x40, 0x57, 0x6f);
pub(crate) const ACCENT: Color32 = Color32::from_rgb(0x26, 0xbb, 0xff);
pub(crate) const WARNING: Color32 = Color32::from_rgb(0xff, 0xb8, 0x00);
pub(crate) const ERROR: Color32 = Color32::from_rgb(0xef, 0x35, 0x35);
pub(crate) const SUCCESS: Color32 = Color32::from_rgb(0x1f, 0xe4, 0x4b);

/// Installs the desktop theme before the first application frame.
pub(crate) fn install(context: &Context) {
    let mut visuals = Visuals::dark();
    let radius = CornerRadius::same(2);
    visuals.override_text_color = Some(FOREGROUND);
    visuals.weak_text_color = Some(Color32::from_rgb(0x80, 0x80, 0x80));
    visuals.selection.bg_fill = SELECTED;
    visuals.selection.stroke = Stroke::new(1.0, PRIMARY_HOVER);
    visuals.hyperlink_color = ACCENT;
    visuals.faint_bg_color = Color32::from_rgb(0x2a, 0x2a, 0x2a);
    visuals.extreme_bg_color = RECESSED;
    visuals.text_edit_bg_color = Some(RECESSED);
    visuals.code_bg_color = Color32::from_rgb(0x1a, 0x1a, 0x1a);
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = ERROR;
    visuals.window_corner_radius = radius;
    visuals.menu_corner_radius = radius;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0, RECESSED);
    visuals.panel_fill = TITLE;
    visuals.button_frame = true;
    visuals.collapsing_header_frame = true;
    visuals.striped = true;
    visuals.widgets.noninteractive = widget(PANEL, PANEL, RECESSED, FOREGROUND, radius);
    visuals.widgets.inactive = widget(PANEL, PANEL, SECONDARY, FOREGROUND, radius);
    visuals.widgets.hovered = widget(SECONDARY, SECONDARY, HOVER, FOREGROUND_HEADER, radius);
    visuals.widgets.active = widget(SELECTED, SELECTED, PRIMARY, FOREGROUND_HEADER, radius);
    visuals.widgets.open = widget(HEADER, HEADER, OUTLINE, FOREGROUND_HEADER, radius);

    context.set_visuals(visuals);
    context.all_styles_mut(|style| {
        style.text_styles = BTreeMap::from([
            (
                TextStyle::Heading,
                FontId::new(20.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            ),
        ]);
        style.spacing.item_spacing = Vec2::new(6.0, 5.0);
        style.spacing.button_padding = Vec2::new(8.0, 4.0);
        style.spacing.interact_size = Vec2::new(40.0, 26.0);
        style.spacing.combo_width = 210.0;
        style.spacing.text_edit_width = 220.0;
        style.spacing.icon_width = 14.0;
        style.spacing.icon_width_inner = 8.0;
        style.spacing.indent = 14.0;
        style.animation_time = 0.08;
        style.compact_menu_style = true;
    });
}

fn widget(
    bg_fill: Color32,
    weak_bg_fill: Color32,
    border: Color32,
    foreground: Color32,
    corner_radius: CornerRadius,
) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill,
        weak_bg_fill,
        bg_stroke: Stroke::new(1.0, border),
        corner_radius,
        fg_stroke: Stroke::new(1.0, foreground),
        expansion: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_type_scale_keeps_body_and_secondary_text_readable() {
        let context = Context::default();
        install(&context);
        let style = context.style_of(eframe::egui::Theme::Dark);
        let has_minimum_size = |text_style, minimum| {
            style
                .text_styles
                .get(&text_style)
                .is_some_and(|font| font.size >= minimum)
        };

        assert!(has_minimum_size(TextStyle::Heading, 20.0));
        assert!(has_minimum_size(TextStyle::Body, 14.0));
        assert!(has_minimum_size(TextStyle::Button, 14.0));
        assert!(has_minimum_size(TextStyle::Monospace, 13.0));
        assert!(has_minimum_size(TextStyle::Small, 12.0));
        assert!(style.spacing.interact_size.y >= 26.0);
    }
}
