# Radar RS Analyst

Radar RS Analyst is a native Rust Level II radar viewer focused on fast, high-fidelity CPU rendering. It loads public NEXRAD Level II data, shows a lightweight map-style radar canvas, and keeps the native super-resolution pixel pattern visible for analyst workflows.

This is an early base release. The priority is a small, fast desktop app that preserves radar detail rather than a broad feature clone.

## Download

Windows users can download the latest `radar-rs-analyst-windows-x64.zip` from the GitHub Releases page, unzip it, and run `radar-rs-analyst.exe`.

## Features

- Native Level II Archive II decode for public NEXRAD data.
- Full-resolution CPU viewport rendering, with no quality downgrade at zoom.
- Reflectivity, velocity, storm-relative velocity, spectrum width, ZDR, RHO, PHI, and CFP product selection when present in the scan.
- Lightweight vector basemap with state/county boundaries and zoom-aware town/county labels.
- Radar-site selection and right-click nearest-site loading.
- VROT/source-gate readout for velocity interrogation.
- Honest timing readout split into lookup, fetch, read, decode, render, worker, texture, and cache stages.
- Cache-aware latest-scan loading so repeated loads are fast while still allowing new scans to download.

## Notes

- First-time latest-scan loads depend on public NOAA/AWS network latency and file size. Repeated loads use the local cache when possible.
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
