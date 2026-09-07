//! `argos-gui`: a single window over the same write flow the CLI runs.

mod app;
mod devices;
mod state;

use std::sync::Arc;

fn main() -> eframe::Result {
    let platform = Arc::from(argos_session::boxed_platform());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 560.0])
            .with_min_inner_size([520.0, 460.0])
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
