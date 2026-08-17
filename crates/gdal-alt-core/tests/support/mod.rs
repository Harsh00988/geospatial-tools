use std::path::PathBuf;

pub fn footprint_fixture_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.join("../..");
    workspace.join("test_data/footprint")
}

pub fn footprint_fixture(name: &str) -> PathBuf {
    footprint_fixture_root().join(name)
}

pub fn footprint_golden(name: &str) -> PathBuf {
    footprint_fixture_root().join("golden").join(name)
}

pub fn resolve_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

pub fn require_footprint_fixture(name: &str) -> PathBuf {
    let path = footprint_fixture(name);
    assert!(
        path.is_file(),
        "missing committed fixture {} (run scripts/generate_footprint_fixtures.sh)",
        path.display()
    );
    path
}
