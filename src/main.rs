mod app;
mod headless;
mod view;

fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "--headless-screenshot" || a == "--headless-benchmark")
    {
        let benchmark = args[0] == "--headless-benchmark";
        if args.len() != 3 {
            eprintln!(
                "Usage: simptxt --headless-screenshot|--headless-benchmark output.png input-file"
            );
            std::process::exit(2);
        }
        if let Err(error) = headless::screenshot(
            std::path::Path::new(&args[1]),
            std::path::Path::new(&args[2]),
            benchmark,
        ) {
            eprintln!("Headless capture failed: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let smoke = std::env::args_os().any(|a| a == "--smoke-test");
    let path = if smoke {
        None
    } else {
        std::env::args_os().nth(1).map(std::path::PathBuf::from)
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("SimpTxt")
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([760.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SimpTxt",
        options,
        Box::new(move |cc| Ok(Box::new(app::EditorApp::new(cc, path, smoke)))),
    )
}
