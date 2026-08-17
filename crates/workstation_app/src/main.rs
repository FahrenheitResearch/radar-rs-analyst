use std::path::PathBuf;

use eframe::egui;

mod app;
mod load_service;
mod pane_canvas;
mod product;
mod render_service;

fn main() -> eframe::Result {
    let input_path = std::env::args_os().nth(1).map(PathBuf::from);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1500.0, 950.0])
            .with_min_inner_size([960.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Radar Workstation",
        native_options,
        Box::new(move |creation_context| {
            Ok(Box::new(app::WorkstationApp::new(
                creation_context,
                input_path,
            )))
        }),
    )
}
