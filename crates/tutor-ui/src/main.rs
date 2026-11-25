//! chess-tutor: an analysis board where Stockfish evaluates every move you
//! try and Claude explains the engine's verdict in plain language.

mod app;
mod board;
mod worker;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Chess Tutor")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([800.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Chess Tutor",
        options,
        Box::new(|cc| Ok(Box::new(app::TutorApp::new(cc)))),
    )
}

/// Loads a system font that has the chess symbols (U+2654–U+265F) as a
/// fallback. Returns whether one was found; otherwise pieces are drawn as
/// lettered discs.
pub fn install_chess_font(ctx: &egui::Context) -> bool {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/freefont/FreeSerif.ttf",
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
        "C:\\Windows\\Fonts\\seguisym.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "chess-symbols".into(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("chess-symbols".into());
            }
            ctx.set_fonts(fonts);
            return true;
        }
    }
    false
}
