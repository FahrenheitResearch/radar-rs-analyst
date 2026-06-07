use std::path::PathBuf;

use nexrad_io::decode_volume_from_path;
use radar_core::MomentStorage;

fn main() {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: cargo run -p nexrad_io --example inspect -- <level2-file>");
        std::process::exit(2);
    };

    match decode_volume_from_path(&path) {
        Ok(volume) => {
            println!("site: {}", volume.site.id);
            println!("volume_time: {}", volume.volume_time);
            if let Some(vcp) = &volume.vcp {
                println!("vcp: {}", vcp.pattern);
            }
            println!(
                "messages: {} decoded_radials: {} skipped_messages: {}",
                volume.metadata.message_count,
                volume.metadata.decoded_radial_count,
                volume.metadata.skipped_message_count
            );
            println!("cuts: {}", volume.cuts.len());
            for (index, cut) in volume.cuts.iter().enumerate() {
                let moments = cut
                    .moments
                    .values()
                    .map(|grid| {
                        let (storage, bytes_per_gate) = match &grid.storage {
                            MomentStorage::U8(_) => ("u8", 1),
                            MomentStorage::U16(_) => ("u16", 2),
                            MomentStorage::F32(_) => ("f32", 4),
                        };
                        let gate_bytes = grid
                            .radial_count()
                            .saturating_mul(grid.gate_range.gate_count)
                            .saturating_mul(bytes_per_gate);
                        format!(
                            "{}:{}x{} {storage} {} KiB",
                            grid.moment.short_name(),
                            grid.radial_count(),
                            grid.gate_range.gate_count,
                            gate_bytes / 1024
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "  cut #{index}: elev={:.2} deg radials={} moments=[{}]",
                    cut.elevation_deg,
                    cut.radials.len(),
                    moments
                );
            }
        }
        Err(err) => {
            eprintln!("decode failed: {err}");
            std::process::exit(1);
        }
    }
}
