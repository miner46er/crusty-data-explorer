//! macOS unified toolbar: the traffic lights sit inside the app's top row.
//!
//! eframe maps these [`egui::ViewportBuilder`] flags to winit on macOS only,
//! so setting them unconditionally is a no-op off macOS.

use eframe::egui;

/// Height of the drag pad reserving sidebar space for the traffic lights.
pub const TRAFFIC_LIGHT_PAD: f32 = 28.0;

/// Whether the unified toolbar is active: macOS, outside fullscreen where
/// the lights hide until hovered.
pub fn unified_titlebar(ctx: &egui::Context) -> bool {
    ctx.os() == egui::os::OperatingSystem::Mac
        && !ctx.input(|input| input.viewport().fullscreen.unwrap_or(false))
}

/// Viewport flags for the unified toolbar.
pub fn viewport() -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_fullsize_content_view(true)
        .with_title_shown(false)
        .with_titlebar_shown(false)
}

/// Reserve sidebar space for the traffic lights; doubles as the window drag
/// handle. Returns `None` off macOS, drawing nothing.
pub fn traffic_light_pad(ui: &mut egui::Ui) -> Option<egui::Response> {
    if !unified_titlebar(ui.ctx()) {
        return None;
    }
    let response =
        ui.allocate_response(egui::vec2(ui.available_width(), TRAFFIC_LIGHT_PAD), egui::Sense::DRAG);
    if response.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_flags_hide_the_os_titlebar() {
        let built = viewport();
        assert_eq!(built.fullsize_content_view, Some(true));
        assert_eq!(built.title_shown, Some(false));
        assert_eq!(built.titlebar_shown, Some(false));
    }

    #[test]
    fn predicate_only_on_macos_windowed() {
        let ctx = egui::Context::default();
        ctx.set_os(egui::os::OperatingSystem::Mac);
        assert!(unified_titlebar(&ctx));
        ctx.set_os(egui::os::OperatingSystem::Nix);
        assert!(!unified_titlebar(&ctx));
        ctx.set_os(egui::os::OperatingSystem::Windows);
        assert!(!unified_titlebar(&ctx));
    }

    #[test]
    fn pad_height_clears_the_lights() {
        assert!(TRAFFIC_LIGHT_PAD >= 28.0);
    }
}
