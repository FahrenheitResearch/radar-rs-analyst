# Radar RS Analyst

Radar RS Analyst is a native Rust Level II radar viewer focused on fast, high-fidelity CPU rendering. It loads public NEXRAD Level II data, shows a lightweight map-style radar canvas, overlays live NWS/SPC hazards, and keeps the native super-resolution pixel pattern visible for analyst workflows.

This is early analyst software. The priority is a small, fast desktop app that preserves radar detail and keeps interaction fluid while the feature surface grows.

## Download

Windows users can download the latest `radar-rs-analyst-windows-x64.zip` from the GitHub Releases page, unzip it, and run `radar-rs-analyst.exe`.

## Features

- Native Level II Archive II decode for public NEXRAD data.
- Public realtime Level II chunk loading for faster access to in-progress scans when available.
- Full-resolution CPU viewport rendering, with no quality downgrade at zoom.
- Reflectivity, velocity, dealiased velocity, storm-relative velocity, spectrum width, ZDR, RHO, PHI, and CFP product selection when present in the scan.
- Built-in reflectivity and velocity color tables plus user-imported `.pal`/`.pal3` color tables.
- Lightweight vector basemap with state/county boundaries and zoom-aware town/county labels.
- Live NWS warnings/advisories, SPC mesoscale discussions, and watch-style hazard polygons with click details.
- Radar-site selection and right-click nearest-site loading without forcing the map to recenter.
- VROT/source-gate readout for velocity interrogation.
- Arrow-key product and tilt stepping for fast analyst workflows.
- Honest timing readout split into lookup, fetch, read, decode, render, worker, texture, and cache stages.
- Cache-aware latest-scan loading so repeated loads are fast while still allowing new scans to download.

## Notes

- First-time latest-scan loads depend on public NOAA/AWS network latency and file size. Repeated loads use the local cache when possible.
- Live hazard and realtime Level II availability depends on upstream public endpoints.
- Dealiasing is conservative and lightweight; it is designed to improve obvious wraps without hiding raw velocity access.
- The app is experimental analyst software, not an official NWS tool, and should not be used as the only source for life-safety decisions.
- Public Level II data availability and latency are controlled by upstream data providers.

## Build From Source

Install a recent stable Rust toolchain, then run:

```powershell
cargo build --release -p app_ui --bin radar-rs-analyst
```

The executable will be written to:

```text
target\release\radar-rs-analyst.exe
```

Run it with an optional local Level II file path:

```powershell
target\release\radar-rs-analyst.exe C:\path\to\KTLX20260607_162229_V06
```

## Performance Workflows

For a fastest local build on the machine that will run the app, build with native CPU instructions:

```powershell
$env:RUSTFLAGS="-C target-cpu=native"
cargo build --release -p app_ui --bin radar-rs-analyst
```

Do not use `target-cpu=native` for broadly distributed binaries unless the target CPU family is controlled.

The renderer probe measures cold decode, preview-to-final decode, direct viewport render, sample-cache build/render/reuse, DVEL, SRV, DSRV, and multiple viewport sizes:

```powershell
cargo run --release -p render2d --example perf_probe -- --runs 12 --decode-runs 8 --viewport 1320x820 C:\path\to\level2-file
```

Use that probe as the training/check workload for PGO or renderer backend experiments. The CPU renderer remains the deterministic correctness path; a future GPU backend should beat these same probe stages without changing radar pixel semantics.

## Repository Layout

- `crates/app_ui` - desktop UI and interaction logic.
- `crates/data_source` - public radar site lookup and Level II object download/cache helpers.
- `crates/nexrad_io` - Level II decode path.
- `crates/render2d` - high-fidelity CPU viewport renderer and perf probes.
- `crates/radar_core` - shared radar volume, cut, radial, and moment data structures.
- `tools/generate_basemap_data.py` - regenerates the baked vector basemap data.

## Basemap Data

The baked basemap uses public US Census 2024 cartographic state/county boundaries and Natural Earth populated places. The source archives are not committed; the generated Rust module is committed so the app builds without runtime map downloads.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license

at your option.
