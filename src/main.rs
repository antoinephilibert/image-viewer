mod app;
mod formats;
mod loader;
mod tree;
mod viewer;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    let (root, select) = resolve_target(arg);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("vu")
            .with_title("vu")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "vu",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, root, select)))),
    )
}

/// Déduit le dossier racine et l'éventuel fichier à sélectionner.
/// Un simple test de métadonnées : aucun décodage avant l'ouverture de la fenêtre.
fn resolve_target(arg: Option<PathBuf>) -> (PathBuf, Option<PathBuf>) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = match arg {
        Some(p) if p.is_absolute() => p,
        Some(p) => cwd.join(p),
        None => return (cwd, None),
    };
    let path = path.canonicalize().unwrap_or(path);
    if path.is_dir() {
        (path, None)
    } else {
        let parent = path.parent().map(PathBuf::from).unwrap_or(cwd);
        (parent, Some(path))
    }
}
