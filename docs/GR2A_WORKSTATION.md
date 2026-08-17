# Radar Workstation: GR2Analyst-Class Product Contract

Status: **accepted foundation contract**

This repository is the clean successor to BowEcho for professional Level II radar analysis. BowEcho remains the broad weather-workstation and experimentation repository. Radar Workstation is deliberately narrower: every dependency, screen, background job, cache, and product must serve radar analysis.

The goal is not a smaller BowEcho window. The goal is a new, maintainable native Rust application that competes directly with GR2Analyst while preserving the fastest and most reliable radar code already developed in BowEcho and radar-rs-analyst.

## 1. Product identity

Radar Workstation is a native, keyboard-first Level II analysis application for live operations, archive review, research, and training.

It must be:

- fast enough that navigation never feels coupled to decoding, product derivation, warning count, or history depth;
- trustworthy about data time, scan completeness, source, tilt, product provenance, and algorithm state;
- useful with one pane, but designed around one, two, and four linked or independent panes;
- fully capable without a cloud account or proprietary data feed;
- maintainable by keeping radar data, algorithms, rendering, runtime scheduling, and egui composition in separate crates;
- extensible through registered products, overlays, tools, and commands rather than feature flags scattered through the app state.

## 2. Hard scope firewall

### In scope

- NEXRAD Level II live chunks, completed live volumes, public archive volumes, local files, local directories, and explicit user URLs.
- Standard, super-resolution, and dual-polarization moments.
- TDWR and research/international polar formats when they fit the same radar-domain contracts.
- Radar-derived sweep, volume, and temporal products.
- Radar inspection, measurement, storm motion, velocity analysis, cross-sections, and three-dimensional volume analysis.
- Radar-relevant overlays: warnings, watches, Local Storm Reports, storm attributes, placefiles, shapefiles, boundaries, roads, labels, user annotations, and range/beam tools.
- Color tables, opacity/transfer functions, product definitions, workspaces, hotkeys, screenshots, and data export.
- Small environmental inputs required by radar algorithms, such as freezing and melting levels or a wind profile. These are inputs to radar analysis, not a general sounding/model workstation.

### Out of scope

The following do not belong in this application unless this contract is deliberately amended:

- numerical weather model browsing or rendering;
- satellite imagery and simulated satellite;
- WRF/ArWen configuration, execution, or post-processing;
- general-purpose sounding analysis;
- tropical-model viewers, climate tools, formula laboratories, social/community feeds, peer caches, or event-storytelling systems;
- flight simulation, storm-world exploration, or unrelated visualization experiments;
- a general GIS editor.

A useful radar-adjacent feature is not automatically a core feature. It must fit an existing product, overlay, tool, or data-source interface and have an explicit memory, scheduling, and UI owner.

## 3. Competitive feature contract

The following is the complete product target. Milestones control implementation order; they do not weaken the end-state contract.

### 3.1 Data acquisition and archive

- Public NEXRAD Level II real-time chunk polling with partial-volume display.
- Automatic transition from preview/partial data to the immutable completed volume.
- Public Level II archive browsing by radar, date, and time.
- NCEI/archive fallback and resilient source priority.
- Local Level II file open, drag-and-drop, command-line open, and directory watch.
- Explicit URL polling and authenticated-provider adapters without provider logic leaking into the UI.
- Persistent, bounded on-disk cache with resume, integrity checks, age limits, and per-site limits.
- Clear source and completeness labels: preview, live partial, live complete, archive, local, and failed/stale.
- Optional compatible polar formats through the shared decoder router: CfRadial, ODIM_H5, DORADE, and other formats already supported by the decoder crates.
- Site catalog with searchable identifiers, names, coordinates, favorites, recent sites, nearest-site selection, and fast warning-to-nearest-radar navigation.

### 3.2 Base products and display modes

- Reflectivity, velocity, spectrum width, differential reflectivity, correlation coefficient, differential phase, and specific differential phase.
- Every available elevation cut, including repeated/Sails cuts, with unambiguous elevation and cut identity.
- Base velocity, dealiased velocity, storm-relative velocity, and user storm motion.
- Range-folded and missing-data treatment that is visible and configurable rather than silently recolored.
- Nearest-bin, smoothed, and high-quality interpolated display modes.
- Standard and extended range views where the source data supports them.
- Palette import, editing, family assignment, velocity polarity, discrete/continuous sampling, and per-product defaults.
- Per-pane opacity, smoothing, gate filters, and quality-control state.

### 3.3 Derived products

The registry must support three distinct kinds of product and must never mislabel one as another:

- **Sweep products:** KDP, texture fields, attenuation-corrected fields, hydrometeor/meteorological masks, azimuthal shear, radial divergence, rotation diagnostics, gust/MARC-style fields, and other products derived from one cut.
- **Volume products:** composite and low-level composite reflectivity, CAPPI, echo top/base/depth, VIL, VIL density, SHI, MESH/MEHS, POSH, POH, height of maximum reflectivity, and column maximum/minimum/mean.
- **Temporal products:** volume difference, trend, maximum/minimum swaths, accumulation, threshold duration, exceedance probability, and maximum-value trails.

Derived products must expose their inputs, algorithm/version, units, grid geometry, quality state, and update behavior. Products that can update from a partial volume should do so without presenting partial output as final.

### 3.4 User-defined products

- Text-based user product definitions with a stable, documented language or expression graph.
- Access to base moments, registered derived fields, height, temperature/freezing levels, and vertical-column reductions.
- Compilation/validation before execution, deterministic resource limits, no unrestricted filesystem or network access, and useful diagnostics.
- Product metadata, palette family, units, valid ranges, and provenance.
- CPU execution first; optional GPU/WebGPU execution may be added behind the same contract.
- Saved presets and reload-on-change for development.

### 3.5 Timeline and looping

- Byte-budgeted immutable volume history, not an unbounded list of decoded objects or textures.
- Configurable frame count and memory limits.
- Live-edge following, pause, go-live, previous/next frame, first/last frame, wrap, end hold, speed, and frame stepping.
- Loop only advances when the destination pane render is ready; it must hold rather than flash, blank, or jitter.
- Archive range loading with bounded concurrency, cancellation, progress, and chronological installation.
- Product/tilt capability is evaluated per frame. Missing data is an explicit unavailable state, never a stale texture presented as current.
- Data-time synchronization for warnings, LSRs, placefiles, and other temporal overlays.

### 3.6 Pane and workspace behavior

- One, two-horizontal, two-vertical, and four-pane layouts.
- Independent product, tilt, smoothing, opacity, storm motion, and overlay visibility per pane.
- Camera link groups rather than a single global linked/unlinked boolean.
- Optional linked time, linked tilt, linked product family, and linked cursor/readout.
- Active-pane keyboard routing with an unmistakable active border.
- Saved workspaces for common severe, winter, tropical, and research layouts.
- Workspace serialization must contain user intent, not live textures, worker handles, or cache internals.

### 3.7 Map, overlays, and placefiles

- High-resolution basemap with state, county, coastline, roads, cities, radar sites, labels, and configurable layer order.
- Online tiles may supplement the map, but basic radar analysis must remain usable offline.
- Live and archive severe weather warnings with full statement history and click selection.
- Watches, mesoscale discussions, and Local Storm Reports where data access is public and reliable.
- Storm-attribute symbols and labels with explicit provenance; locally derived detections must not be visually confused with NEXRAD/NWS products.
- GRLevelX-compatible placefiles, including timed objects, icons, polygons, lines, text, thresholds, and refresh behavior.
- User shapefiles/GeoJSON and simple annotations.
- Home marker, temporary markers, ruler, range rings, azimuth/range readout, beam-center height, latitude/longitude, and sampled value.
- Deterministic label collision/budget behavior so active weather cannot make interaction collapse.

### 3.8 Analyst tools

- Cursor sampling with product value, units, azimuth, slant/ground range, latitude/longitude, beam height, and source time.
- Marker-to-cursor measurements and multiple saved markers.
- Vrot/delta-V measurement with configurable gates, traceable sample locations, and dealiased/raw choice.
- Storm-motion editor and Bunkers/VWP-assisted motion suggestions when input quality allows.
- Rotation, shear, divergence, cell, and track inspection with confidence/quality diagnostics.
- VWP with accepted/rejected levels and reasons.
- Two-point vertical cross-section, draggable endpoints, position/swing controls, smoothing, and export.
- RHI support when the source scan is an RHI.
- Time-height and trend plots for a point, marker, or tracked object.
- Optional multi-radar overlay and compositing with age, beam height, distance, and coverage controls.

### 3.9 Three-dimensional analysis

- Selection of a storm region from a 2D pane.
- Lit semi-transparent volume mode and isosurface mode.
- Base and compatible derived products.
- Editable transfer/alpha functions with saved presets.
- Storm-motion correction across the scan period.
- Camera orbit, tilt, zoom, clipping, vertical exaggeration, lighting, and sampling controls.
- Cross-section plane shown in the 3D context.
- Honest treatment of missing volume coverage and cone-of-silence geometry.
- Rendering behind a dedicated GPU scene interface; the egui update thread must never build volume geometry.

### 3.10 Export, configuration, and operations

- Screenshot/copy image with optional legend, timestamp, source, site, product, and overlay attribution.
- Export sampled values, cross-sections, tracks, and compatible derived grids to documented formats.
- Command palette plus fully remappable keyboard shortcuts.
- Atomic settings writes, versioned migrations, safe reset, and separate cache/settings roots.
- Diagnostic panel for decode, product, render, texture-upload, network, queue, cache, and frame-time measurements.
- Crash-safe logs without telemetry by default.
- Cross-platform desktop support, with Windows as the primary release target and Linux/macOS kept buildable where backend support permits.

## 4. Deliberate advantages over the incumbent

Parity is the floor. Radar Workstation should also preserve advantages already demonstrated by the Rust stack:

- display useful partial-volume data while a live scan is still arriving;
- decode modern Level II compression in a bounded, parallel pipeline;
- retain raw compact moment storage rather than expanding every gate into heap objects;
- offer multiple velocity-dealiasing algorithms and quality diagnostics;
- support more than one radar layer without one worker thread per layer;
- run as a native cross-platform application with no administrator-only registration state;
- expose exact data time, partial/complete state, algorithm provenance, and performance timings;
- keep all queues, histories, caches, and texture stores explicitly bounded.

## 5. Repository architecture

The existing high-performance crates are retained where their contracts are already clean:

- `radar_core`: compact, immutable radar-domain objects and geometry.
- `nexrad_io`: bounded decoder/router for Level II and compatible formats.
- `data_source`: acquisition primitives, site catalogs, archive/live listing, download, resume, and disk cache. Radar Workstation imports only radar-source modules.
- `color_tables`: palette parsing and sampling.
- `render2d`: CPU rasterizer and mature radar algorithms. Algorithms may later be split from rasterization without changing product contracts.
- `product_engine`: sweep/volume/temporal product registry and derivation.
- `timeline`: legacy placeholder; it will either be replaced by or migrated into `analyst_runtime`.

New boundaries:

- `analyst_runtime`: UI-independent workspace state, pane state, bounded history, generation tokens, coalescing queues, command model, and camera transforms.
- `map_scene`: retained world-space map/overlay data, LOD selection, triangulation, label placement, and the GPU paint callback. This crate must not depend on `app_ui`.
- `workstation_app`: thin egui composition, menus, panels, dialogs, shortcut routing, texture installation, and dependency wiring.
- `volume_scene`: optional 3D volume renderer behind a narrow interface.

`workstation_app` is the composition root. It must not become the implementation location for decoders, radar algorithms, map projection, history policy, worker queues, or product derivation.

## 6. State model

### Immutable radar snapshots

Decoded volumes are installed as `Arc<RadarVolume>`. A preview, partial update, or completed replacement is a new snapshot with an explicit stage. Workers never mutate the object currently displayed by the UI.

### Workspace

A workspace owns:

- layout and active pane;
- up to four pane intents;
- camera-link groups and optional time/tilt/product link groups;
- shared history cursor/live-edge state;
- overlay configuration and tool state;
- serializable user preferences.

It does not own worker threads, channels, GPU textures, decoded cache implementation, or network clients.

### Pane

A pane intent owns product selection, tilt selection, camera, display quality, storm motion, opacity, visible overlays, cursor/tool state, and link membership. Runtime render state is kept separately so a workspace can be serialized safely.

### Generation tokens

Every asynchronous domain has a monotonically increasing generation:

- source/site session;
- history selection;
- pane product/tilt;
- camera/view;
- palette/display settings;
- overlay dataset.

A result is installed only when all generations relevant to that result still match. String labels and raw pointer identity are never the sole stale-result guard.

## 7. Rendering architecture

### Radar raster

The proven CPU raster path remains the first production renderer. It consumes immutable compact moments and emits a viewport RGBA image. It may retain row lookup, palette, geometry, and sample caches inside worker-owned state.

During a gesture, the last valid radar texture is transformed as a single quad immediately. A newest-wins request for the exact camera is processed off the UI thread. The old texture is replaced only by a matching-generation result.

A future GPU polar renderer is permitted, but it must implement the same pane/render contract and pass image/measurement parity tests before becoming default.

### Map and geographic overlays

The map pipeline must not repeat BowEcho's camera-coupled CPU-shape design.

Rules:

1. Source geometry is parsed once into stable geographic or projected world-space buffers.
2. Projection to the scene coordinate system occurs at dataset build time, not on every pan frame.
3. Camera motion is an affine/uniform transform applied by the renderer.
4. Cache identity is dataset generation + projection + style + LOD. Raw camera `f32` bits are never part of geometry identity.
5. Panning does not rebuild or clone polygon vertices.
6. Zoom chooses an LOD bucket with hysteresis; it does not create a distinct cache entry for every wheel delta.
7. Line widths, dashes, icons, and labels that are screen-space concerns are resolved in the renderer or a bounded label pass.
8. Polygon triangulation and line tessellation occur off the egui thread.
9. Every dataset and GPU buffer cache has a byte owner and eviction policy.
10. The fallback CPU path uses quantized camera buckets and retained world geometry; it may not become the primary architecture.

The preferred implementation is a custom wgpu paint callback embedded in egui. Radar textures, map geometry, warning polygons, placefiles, and annotations remain separate render layers with deterministic ordering.

### Labels

Labels are candidates derived from retained features. Each frame runs a bounded placement pass over visible candidates, ordered by priority and distance. The pass has hard candidate and time budgets, stable placement across small camera changes, and no unbounded all-pairs collision work.

## 8. Runtime scheduling

The UI thread may classify input, update intent, drain bounded result queues, upload a bounded number of textures, and compose egui. It may not download, decode, derive, reproject, triangulate, rasterize, or scan all history frames.

Required lanes:

- `Interactive(PaneId)`: newest request wins independently for each visible pane.
- `Overlay(OverlayId)`: newest request wins per overlay; a bounded shared pool prevents overlay bursts from starving interactive work.
- `Archive`: bounded distinct jobs with cancellation and chronological installation.
- `Prewarm(PaneId, FrameKey)`: speculative loop renders under an explicit in-flight and byte limit.
- `MapBuild(DatasetId)`: parse/project/tessellate off-thread, then upload immutable buffers.
- `Volume3d(PaneId)`: isolated from the 2D interactive worker.

Results are drained under a per-frame time and upload budget. If a queue still contains results, another repaint is requested; the app never drains an unbounded burst in one egui frame.

## 9. Performance and memory gates

These are release gates, not aspirations:

- Pan and zoom input must update the screen in the same egui frame using retained transforms.
- A pan frame performs no geographic reprojection and no polygon/line tessellation.
- The UI-thread p95 frame must stay below 16.7 ms during interaction on the reference machine; the target is below 8 ms.
- Active-warning count must not materially change pan cost once the warning dataset is resident.
- There is at most one queued interactive request per visible pane.
- Worker results that are already stale are dropped before texture upload.
- Texture upload is bounded per frame.
- Default decoded-history budget is 1 GiB on desktop, configurable and enforced by estimated resident bytes as well as frame count.
- Radar texture, map tile, retained geometry, derived-product, sample-cache, and prewarm stores each have independent byte/count budgets.
- Switching sites drops the old session and makes its queued results uninstallable.
- No cache uses raw camera-center or scale bits as an unbounded key.
- Four-pane looping must hold for missing renders rather than allocate indefinitely or display the wrong frame.
- All network reads and compressed inputs have explicit size and time limits.

Benchmarks must include:

- far-out panning with hundreds of active warning polygons;
- one/two/four-pane live operation;
- a 30-frame loop with mixed product capabilities;
- repeated site switching while downloads and renders are in flight;
- a large super-resolution volume and a difficult velocity-dealias case;
- map zoom across every LOD boundary;
- placefiles with many lines, polygons, icons, and labels;
- 3D selection and camera interaction when the volume renderer lands.

## 10. Maintainability gates

- `main.rs` is startup only and should remain below 300 lines.
- No hand-written application module should exceed 2,000 lines without an approved split plan. Generated map/site data is exempt and must be clearly marked.
- Product behavior is registered through descriptors and evaluators, not `match` arms copied across UI, renderer, history, and export code.
- Data sources implement one source contract and report typed states/errors.
- UI panels emit commands; they do not launch ad-hoc threads.
- Runtime policies have pure/unit-tested cores.
- Every background job has cancellation/staleness semantics.
- Every cache has a documented key, owner, bound, and invalidation rule.
- App dependencies are checked by a firewall test. Model, satellite, WRF, and other BowEcho-only crates are forbidden.
- New features require tests for stale results, site switch, missing capability, memory ownership, and keyboard routing where applicable.
- Unsafe code is confined to reviewed decoder/GPU boundaries and documented.

## 11. Operator interface

The default workspace is intentionally simple:

- compact top command bar: site/source, product, tilt, layout, link state, tools, and command search;
- central one/two/four-pane radar canvas with no permanent oversized settings sidebar;
- slim bottom timeline/transport strip with exact data time and live/paused/partial state;
- transient inspector on hover/click;
- focused tool windows for warnings, product definitions, color tables, cross-sections, and 3D volume work;
- right-click context actions for marker, home, measurement, cross-section, volume selection, and copy/export;
- keyboard-first product, tilt, frame, pane, site, and tool navigation.

Advanced controls should be discoverable without occupying the radar canvas continuously. A feature that needs a permanent panel must justify that screen cost.

## 12. Delivery milestones

### M0 — Foundation

- Accept this contract.
- Establish `analyst_runtime` with bounded history, generation tokens, coalescing lanes, pane/workspace intent, and camera transforms.
- Add dependency firewall and architecture tests.
- Keep the existing viewer available as a behavior/reference harness.

### M1 — Clean workstation shell

- New thin `workstation_app` composition root.
- One/two/four panes, active-pane routing, camera link groups, product/tilt controls, local-file loading, and exact status truth.
- One shared interactive render service with per-pane newest-wins requests.

### M2 — Retained map scene

- Extract map data from application code.
- World-space retained geometry, LOD buckets, GPU paint callback, tile support, range rings, markers, and readout.
- Warning-polygon stress benchmark proving camera-independent pan cost.

### M3 — Live/archive and history

- Real-time chunks, previews, completed replacement, archive browser, backfill, disk cache, cancellation, byte-budgeted history, and ready-gated looping.

### M4 — 2D analyst parity

- Full base/dual-pol product set, dealiased/SRV, derived registry, measurements, VWP, rotation/shear/divergence, color tables, and multi-pane workflows.

### M5 — Operational overlays

- Warnings and statement history, LSRs, storm attributes, placefiles, shapefiles, labels, user annotations, and time synchronization.

### M6 — Cross-sections and volume analysis

- 2D/3D cross-sections, time-height tools, lit volume, isosurfaces, transfer functions, storm-motion correction, and export.

### M7 — User products and temporal analysis

- Sandboxed user-defined product language, maximum-value trails, swaths, trends, accumulations, and saved product packs.

### M8 — Hardening and release

- Migration/import tools, crash/logging polish, updater/packaging, accessibility, complete hotkey editor, corpus/visual regression suite, and performance certification.

## 13. Definition of direct-competitor complete

Radar Workstation reaches the stated target when a user can perform the established GR2Analyst workflows—live and archive Level II viewing, every tilt and dual-pol moment, high-resolution derived products, user-defined products, maximum-value trails, warnings/LSRs, cross-sections, and lit/isosurface volume analysis—without leaving the application, while also meeting the runtime, memory, truth, and maintainability gates above.

Until then, milestones and release notes must describe exactly which parts of the contract are implemented. Marketing labels must not imply parity that the tested feature matrix does not yet support.
