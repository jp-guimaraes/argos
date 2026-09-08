//! `argos-gui`: a single window over the same write flow the CLI runs.

mod app;
mod devices;
#[cfg(target_os = "linux")]
mod linux_theme;
mod state;
mod theme;

use std::sync::Arc;

fn main() -> eframe::Result {
    let platform = Arc::from(argos_session::boxed_platform());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Portrait, and narrow: this is a short vertical form, not a
            // dashboard. Nothing in it wants horizontal room -- the one
            // element that could demand it, the image path, is elided to its
            // file name rather than allowed to set the window's width.
            .with_inner_size(theme::metric::WINDOW_INITIAL)
            .with_min_inner_size(theme::metric::WINDOW_MINIMUM)
            // Matched by StartupWMClass in the .desktop file (#94).
            .with_app_id(app::APP_ID),
        ..Default::default()
    };
    eframe::run_native(
        "Argos",
        options,
        Box::new(|_cc| Ok(Box::new(app::ArgosApp::new(platform)))),
    )
}
