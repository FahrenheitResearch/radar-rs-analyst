# BowEcho Reuse Matrix

This document prevents the new radar workstation from becoming a second copy of BowEcho. The extraction rule is simple: reuse proven radar-domain code and measured algorithms; do not reuse BowEcho's application topology.

## Reuse directly or with a narrow adapter

| Capability | Decision | Boundary |
|---|---|---|
| Compact decoded radar volume, cuts, radials, gate geometry, moments, product identifiers | Reuse | `radar_core`; immutable `Arc<RadarVolume>` snapshots |
| Level II framing, compression, message decode, partial-volume preview | Reuse | `nexrad_io`; bounded input and preview callbacks |
| CPU polar-to-RGBA rasterization, lookup tables, color-table sampling | Reuse | `render2d`; worker-owned caches and viewport requests |
| Velocity dealiasing and storm-relative transforms | Reuse | registered radar products; never UI-owned algorithms |
| Color table parsing and sampling | Reuse | `color_tables`; palette generation participates in render stamps |
| Public Level II live/archive listing, download, atomic cache writes | Reuse radar-only modules | `data_source`; typed source sessions and bounded disk policy |
| Sweep, volume, and temporal derived-product evaluators that already have tests | Reuse behind registry | `product_engine`; descriptors expose inputs, units, stage, and provenance |
| Site metadata and radar-local projection helpers | Reuse data, rewrite ownership | stable catalog and projection services, not copied application fields |
| Warning, LSR, placefile, and boundary parsers that are independent of BowEcho UI state | Extract selectively | immutable overlay datasets consumed by `map_scene` |
| Cross-section, VWP, sampling, and measurement science with independent tests | Extract selectively | analyst tools operating on immutable volumes |

## Rewrite around the retained workstation architecture

| BowEcho behavior | Replacement |
|---|---|
| Geographic geometry projected and emitted as egui `Shape`s for a camera-specific view | Project once into retained world-space buffers; camera movement is a renderer transform |
| Cache identities containing exact camera-center or scale `f32` bits | Dataset + projection + style + hysteretic LOD identity; camera is never geometry identity |
| Shared/global camera-link boolean | Independent camera, time, tilt, product, and cursor link groups |
| Ad-hoc render/download threads created by feature panels | Named bounded runtime lanes with generation-aware results |
| Stale-result guards based on labels, dimensions, or pointer identity | Typed source/frame/pane/view/palette generations |
| Frame-count-only radar history | Immutable chronological history bounded by both frame count and estimated resident bytes |
| Loop advance regardless of render readiness | Ready-gated advance; hold the current frame until all visible destination panes are ready |
| Feature panels owning network clients, caches, algorithms, or worker handles | Panels emit commands; services own work and typed results |
| Map, warning, placefile, and label work competing with radar interaction on the egui thread | Retained scene build lanes and bounded label placement |
| One giant application state containing serializable intent and runtime resources together | Serializable workspace intent separated from textures, workers, queues, and caches |
| Experimental features added directly to the main radar UI | Product, overlay, tool, source, or command registration plus explicit scope review |

## Leave in BowEcho

The following are useful experiments or weather-workstation features, but they are not part of a maintainable GR2Analyst-class radar application:

- numerical weather model browsing, rendering, and diagnostics;
- satellite and simulated-satellite imagery;
- WRF/ArWen configuration or post-processing;
- general-purpose sounding and environmental analysis screens;
- tropical-model products and climate tools;
- flight simulation and storm-world exploration;
- community/social feeds, peer caches, event storytelling, and formula laboratories;
- unrelated web viewers and showcase modes.

A radar algorithm may accept a small environmental input such as a freezing level or wind profile. That does not justify importing a general model or sounding workstation.

## Acceptance test for every extraction

Before BowEcho code enters the new application, the change must answer all of these:

1. Is it radar-domain functionality required by the accepted product contract?
2. Can it live in a crate that does not depend on the egui application crate?
3. Does every asynchronous result have explicit cancellation or generation semantics?
4. Does every cache or retained allocation have a documented owner and bound?
5. Does camera motion avoid geometry rebuild, projection, and unbounded key creation?
6. Are missing, partial, stale, and failed states represented truthfully?
7. Does the feature have focused tests independent of the full GUI?

If any answer is no, the code is redesigned before extraction rather than copied and repaired later.
