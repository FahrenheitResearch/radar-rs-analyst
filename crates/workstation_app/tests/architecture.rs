use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED_DIRECT_DEPENDENCIES: &[&str] = &[
    "analyst_runtime",
    "chrono",
    "color_tables",
    "data_source",
    "eframe",
    // The map scene owns the graphics backend; the workstation depends on it
    // rather than on wgpu, bytemuck or any GIS crate directly.
    "map_scene",
    "nexrad_io",
    "radar_core",
    "render2d",
];
const MAX_MAIN_LINES: usize = 300;
const MAX_MODULE_LINES: usize = 2_000;

#[test]
fn direct_dependencies_stay_inside_the_radar_workstation_firewall() {
    let manifest = include_str!("../Cargo.toml");
    let dependencies = dependency_names(manifest);
    let allowed = ALLOWED_DIRECT_DEPENDENCIES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let unexpected = dependencies
        .difference(&allowed)
        .copied()
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "unexpected direct workstation dependencies: {unexpected:?}"
    );
}

#[test]
fn composition_root_and_modules_stay_bounded() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let main_path = source_root.join("main.rs");
    let main_lines = line_count(&main_path);
    assert!(
        main_lines <= MAX_MAIN_LINES,
        "{} has {main_lines} lines; startup-only main.rs limit is {MAX_MAIN_LINES}",
        main_path.display()
    );

    let mut rust_files = Vec::new();
    collect_rust_files(&source_root, &mut rust_files);
    for path in rust_files {
        let lines = line_count(&path);
        assert!(
            lines <= MAX_MODULE_LINES,
            "{} has {lines} lines; module limit is {MAX_MODULE_LINES}",
            path.display()
        );
    }
}

fn dependency_names(manifest: &str) -> BTreeSet<&str> {
    let mut in_dependencies = false;
    let mut names = BTreeSet::new();
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _value)) = line.split_once('=') {
            // A dependency may be declared as `name = ...` or with a dotted
            // key such as `name.workspace = true`; the crate is the first
            // segment either way.
            let name = key.trim().split('.').next().unwrap_or_default().trim();
            if !name.is_empty() {
                names.insert(name);
            }
        }
    }
    names
}

fn collect_rust_files(directory: &Path, output: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry
            .expect("source directory entry should be readable")
            .path();
        if path.is_dir() {
            collect_rust_files(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
        .lines()
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parser_reads_only_direct_dependency_section() {
        let manifest = r#"
[package]
name = "example"

[dependencies]
a = "1"
b = { path = "../b" }

[dev-dependencies]
c = "1"
"#;
        assert_eq!(dependency_names(manifest), BTreeSet::from(["a", "b"]));
    }

    #[test]
    fn manifest_parser_reads_workspace_inherited_dependencies() {
        let manifest = r#"
[dependencies]
a.workspace = true
b = { path = "../b" }
"#;
        assert_eq!(dependency_names(manifest), BTreeSet::from(["a", "b"]));
    }
}
