use crusty_data_explorer::app::AppState;
use crusty_data_explorer::ui;

fn main() -> eframe::Result {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: crate::ui::chrome::viewport(),
        ..Default::default()
    };
    eframe::run_native(
        "crusty-data-explorer",
        options,
        Box::new(|cc| {
            ui::install(&cc.egui_ctx);
            Ok(Box::new(AppState::new(&cc.egui_ctx)))
        }),
    )
}
