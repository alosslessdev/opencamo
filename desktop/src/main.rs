//! opencamo desktop client — phone camera as a webcam over the LAN.

mod app;
mod loopback;
mod protocol;
mod selftest;
mod stream;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Headless self-test path: must never touch eframe/egui.
    if args.get(1).map(String::as_str) == Some("--selftest") {
        return ExitCode::from(selftest::run(&args[2..]) as u8);
    }

    if args.len() > 1 {
        eprintln!("usage: opencamo-desktop [--selftest <stream-url> <n_frames>]");
        return ExitCode::from(2);
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 700.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("opencamo"),
        ..Default::default()
    };

    match eframe::run_native(
        "opencamo-desktop",
        options,
        Box::new(|cc| Box::new(app::App::new(cc))),
    ) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("eframe error: {}", e);
            ExitCode::FAILURE
        }
    }
}
