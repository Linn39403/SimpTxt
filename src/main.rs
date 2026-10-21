mod app;
mod fonts;
#[cfg(feature = "headless-capture")]
mod headless;
mod highlight;
fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(not(feature = "headless-capture"))]
    if args.first().is_some_and(|a| a == "--screenshot") {
        eprintln!(
            "Screenshot capture is a development feature. Rebuild with --features headless-capture."
        );
        std::process::exit(2);
    }
    #[cfg(feature = "headless-capture")]
    if args.first().is_some_and(|a| a == "--screenshot") {
        let Some(output) = args.get(1) else {
            eprintln!(
                "Usage: rust-notepad-prototype --screenshot output.png [input] [--dark] [--settings]"
            );
            std::process::exit(2)
        };
        let input = args
            .get(2)
            .filter(|s| *s != "--dark" && *s != "--settings")
            .map(std::path::Path::new);
        if let Err(e) = headless::capture(
            std::path::Path::new(output),
            input,
            args.iter().any(|s| s == "--dark"),
            args.iter().any(|s| s == "--settings"),
        ) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let path = args.first().map(std::path::PathBuf::from);
    eframe::run_native(
        "Notepad prototype",
        eframe::NativeOptions {
            #[cfg(feature = "native-glow")]
            renderer: eframe::Renderer::Glow,
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title("Notepad prototype")
                .with_inner_size([1100.0, 720.0])
                .with_min_inner_size([520.0, 320.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            let mut app = app::App::new(&cc.egui_ctx);
            if let Some(path) = path {
                app.open_path(&path);
            }
            Ok(Box::new(app))
        }),
    )
}
