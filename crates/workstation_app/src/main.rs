use std::path::PathBuf;

use eframe::egui;

mod app;
mod live_service;
mod load_service;
mod pane_canvas;
mod product;
mod render_service;

/// Startup intent parsed from the command line: either a Level II file to
/// open, or a four-character site to start a live session for.
struct Startup {
    input_path: Option<PathBuf>,
    live_site: Option<String>,
}

/// `radar-workstation [<level2-file>] [--live <SITE>]`
fn parse_startup<I: Iterator<Item = String>>(args: I) -> Startup {
    let mut input_path = None;
    let mut live_site = None;
    let mut pending_live = false;
    for arg in args {
        if pending_live {
            live_site = Some(arg);
            pending_live = false;
        } else if arg == "--live" {
            pending_live = true;
        } else if let Some(site) = arg.strip_prefix("--live=") {
            live_site = Some(site.to_owned());
        } else if !arg.starts_with("--") && input_path.is_none() {
            input_path = Some(PathBuf::from(arg));
        }
    }
    Startup {
        input_path,
        live_site,
    }
}

fn main() -> eframe::Result {
    let startup = parse_startup(std::env::args().skip(1));
    let Startup {
        input_path,
        live_site,
    } = startup;
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
                live_site,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup(args: &[&str]) -> Startup {
        parse_startup(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn parses_a_bare_file_path() {
        let parsed = startup(&["C:/data/KTLX_V06"]);
        assert_eq!(parsed.input_path, Some(PathBuf::from("C:/data/KTLX_V06")));
        assert_eq!(parsed.live_site, None);
    }

    #[test]
    fn parses_both_live_flag_spellings() {
        assert_eq!(
            startup(&["--live", "KTLX"]).live_site.as_deref(),
            Some("KTLX")
        );
        assert_eq!(startup(&["--live=KTLX"]).live_site.as_deref(), Some("KTLX"));
    }

    #[test]
    fn does_not_treat_a_live_site_as_a_file_path() {
        let parsed = startup(&["--live", "KTLX"]);
        assert_eq!(parsed.input_path, None);
    }
}
