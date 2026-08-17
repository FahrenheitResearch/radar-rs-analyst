use std::path::PathBuf;

use eframe::egui;

mod app;
mod live_service;
mod load_service;
mod pane_canvas;
mod product;
mod render_service;
mod sites_service;

/// Startup intent parsed from the command line: a Level II file to open or a
/// site to go live on, plus an optional starting camera.
///
/// The camera options exist so a given view is reproducible from the command
/// line. Driving this window with synthetic mouse input is unreliable — Windows
/// refuses foreground changes from a background process — so a stated camera is
/// the only honest way to capture a specific pan or zoom.
#[derive(Default)]
struct Startup {
    input_path: Option<PathBuf>,
    live_site: Option<String>,
    zoom_km_per_point: Option<f32>,
    center_km: Option<(f64, f64)>,
}

/// `radar-workstation [<level2-file>] [--live <SITE>] [--zoom <km-per-point>]
/// [--center <east_km,north_km>]`
fn parse_startup<I: Iterator<Item = String>>(args: I) -> Startup {
    let mut startup = Startup::default();
    let mut pending: Option<String> = None;

    for arg in args {
        if let Some(option) = pending.take() {
            apply_option(&mut startup, &option, &arg);
            continue;
        }
        match arg.split_once('=') {
            Some((option, value)) if option.starts_with("--") => {
                apply_option(&mut startup, option, value);
            }
            _ if arg.starts_with("--") => pending = Some(arg),
            _ if startup.input_path.is_none() => startup.input_path = Some(PathBuf::from(arg)),
            _ => {}
        }
    }
    startup
}

fn apply_option(startup: &mut Startup, option: &str, value: &str) {
    match option {
        "--live" => startup.live_site = Some(value.to_owned()),
        "--zoom" => startup.zoom_km_per_point = value.parse().ok(),
        "--center" => {
            if let Some((east, north)) = value.split_once(',')
                && let (Ok(east), Ok(north)) = (east.trim().parse(), north.trim().parse())
            {
                startup.center_km = Some((east, north));
            }
        }
        _ => {}
    }
}

fn main() -> eframe::Result {
    let Startup {
        input_path,
        live_site,
        zoom_km_per_point,
        center_km,
    } = parse_startup(std::env::args().skip(1));
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
            // Register the map's persistent GPU resources once, before any
            // pane paints. Without a wgpu render state the map cannot draw at
            // all, so say so rather than silently falling back to per-frame
            // CPU geometry.
            match creation_context.wgpu_render_state.as_ref() {
                Some(render_state) => {
                    let resources = map_scene::gpu::MapRenderResources::new(
                        &render_state.device,
                        render_state.target_format,
                    );
                    render_state
                        .renderer
                        .write()
                        .callback_resources
                        .insert(resources);
                }
                None => eprintln!(
                    "wgpu map unavailable: no wgpu render state; the basemap will not draw"
                ),
            }

            let mut app = app::WorkstationApp::new(creation_context, input_path, live_site);
            app.set_initial_camera(zoom_km_per_point, center_km);
            Ok(Box::new(app))
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
