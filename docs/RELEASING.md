# Releasing GenericRadar

The public repo (github.com/FahrenheitResearch/GenericRadar) is a **release
snapshot target**. Development happens here; the public repo receives a
history-light snapshot per release. Never develop on the public repo directly.

Two incidents shaped this process; both rules below exist because breaking
them shipped the wrong thing to real users:

1. **Inherited CI is live code.** The first public snapshot was made with a
   bare `git archive HEAD`, which carried this dev repo's
   `.github/workflows/release.yml`. The tag push made GitHub Actions build and
   serve the *legacy* app, under its own name, for six platforms, from the new
   repo. A published snapshot's `.github/` must contain exactly one thing: the
   purpose-built pipeline copied from `docs/release/release.yml` in this repo.
2. **The maintainer flies the exact build before anything ships.** Green gates
   are not approval. The `GenericRadar.exe` asset on every release is the
   byte-identical binary the maintainer ran and approved on their own machine.
   CI supplements it with cross-platform archives built from the same tag; it
   never replaces it.

## Process

1. **Version.** The tag must equal `[workspace.package].version` in
   `Cargo.toml`. Bump it in dev first.
2. **Gates.** `cargo test --release --workspace`, `cargo clippy --release
   --workspace --all-targets -- -D warnings`, `cargo fmt --check` - all green.
3. **Flight.** Build `cargo build --release -p workstation_app --bin
   GenericRadar`, copy the exe to the maintainer's Desktop, and get explicit
   approval of *that* build. Record its SHA-256.
4. **Snapshot.** From a clean HEAD:
   - `git archive HEAD | tar -x -C <staging>`
   - Remove: `.github/`, `tools/`, `crates/app_ui/`, `Cargo.lock`,
     `docs/ANALYST_WORKSTATION.md`, `docs/review-*.md`.
   - Replace `README.md` with the public repo's README (the dev README
     describes the legacy app).
   - Patch `Cargo.toml`'s `repository` URL to the GenericRadar repo.
   - Copy `docs/release/release.yml` to `.github/workflows/release.yml`.
   - **Leak scan** (all must return nothing):
     `grep -riE "the owner|owner's|owner-approved|radar-rs-analyst|users.drew" --include="*.rs" --include="*.md" --include="*.toml" .`
     (ignore `ownership` matches), plus a look at both LICENSE files - the
     copyright holder is `Fahrenheit Research`, never a personal name.
   - Compile-check the staging tree standalone
     (`cargo check --release -p workstation_app --bin GenericRadar`).
5. **Publish.** Clone the public repo, replace its tree with the snapshot
   (keep `.git/`), commit `GenericRadar vX.Y.Z`, push `main`.
6. **Release.** `gh release create vX.Y.Z` with hand-written factual notes
   (what changed for users; unsigned-binary SmartScreen note; data credits:
   NOAA/NWS NEXRAD Level II, api.weather.gov, USGS The National Map,
   OpenStreetMap contributors) and the flown `GenericRadar.exe` as an asset.
   Creating the release creates the tag; the tag fires the pipeline, which
   adds the six platform archives beside the exe. Notes must say non-Windows
   builds are compiled from the same source but not flight-tested.
7. **Verify.** Download the `GenericRadar.exe` asset and compare its SHA-256
   to the flown build; watch the Actions run go green; confirm the published
   tree's `.github/` contains only the intended workflow.

Release notes never narrate design conversations or process history; they say
what the software does.
