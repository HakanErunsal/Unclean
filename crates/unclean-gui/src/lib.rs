#![doc = "Provides the Unclean desktop frontend over the shared product core."]

mod app;
mod theme;
pub mod workflow;

use eframe::egui;

const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../../assets/unclean-icon.png");

/// Opens the desktop engine workflow.
///
/// # Errors
///
/// Returns an error when native window creation or built-in application state fails.
pub fn run_gui() -> eframe::Result<()> {
    let icon = window_icon()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        unclean_core::PRODUCT_NAME,
        options,
        Box::new(|creation_context| {
            Ok(Box::new(app::UncleanApp::new(&creation_context.egui_ctx)?))
        }),
    )
}

fn window_icon() -> eframe::Result<egui::IconData> {
    eframe::icon_data::from_png_bytes(WINDOW_ICON_PNG)
        .map_err(|error| eframe::Error::AppCreation(Box::new(error)))
}

#[cfg(test)]
mod tests {
    use super::window_icon;

    #[test]
    fn embedded_window_icon_has_expected_dimensions() -> eframe::Result<()> {
        let icon = window_icon()?;

        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
        Ok(())
    }
}
