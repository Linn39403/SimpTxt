mod app;
mod headless;
mod preferences;
mod project_tree;
mod view;

fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "--headless-screenshot" || a == "--headless-benchmark")
    {
        let benchmark = args[0] == "--headless-benchmark";
        if args.len() < 3 {
            eprintln!(
                "Usage: simptxt --headless-screenshot|--headless-benchmark output.png input-file \
                 [--light] [--vim] [--folder directory] [--tab file] [--cursor-byte offset]"
            );
            std::process::exit(2);
        }
        let mut options = headless::CaptureOptions::default();
        let mut extra = args[3..].iter();
        while let Some(argument) = extra.next() {
            if argument == "--light" {
                options.theme = preferences::Theme::Light;
            } else if argument == "--vim" {
                options.vim = true;
            } else if argument == "--cursor-byte" {
                options.cursor_byte = extra
                    .next()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse().ok());
                if options.cursor_byte.is_none() {
                    eprintln!("--cursor-byte needs a non-negative byte offset");
                    std::process::exit(2);
                }
            } else if argument == "--folder" || argument == "--tab" {
                let Some(path) = extra.next() else {
                    eprintln!("Missing path after {}", argument.to_string_lossy());
                    std::process::exit(2);
                };
                if argument == "--folder" {
                    options.folder = Some(path.into());
                } else {
                    options.tabs.push(path.into());
                }
            } else {
                eprintln!("Unknown capture option: {}", argument.to_string_lossy());
                std::process::exit(2);
            }
        }
        if let Err(error) = headless::screenshot(
            std::path::Path::new(&args[1]),
            std::path::Path::new(&args[2]),
            benchmark,
            options,
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
        Box::new(move |cc| Ok(Box::new(app::WorkspaceApp::new(cc, path, smoke)))),
    )
}
