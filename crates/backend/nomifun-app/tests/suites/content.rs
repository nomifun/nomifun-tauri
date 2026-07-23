#[path = "../common/mod.rs"]
mod common_support;
pub(crate) use common_support::*;
extern crate self as common;

macro_rules! grouped_tests {
    ($($module:ident => $path:literal),+ $(,)?) => {
        $(
            #[path = $path]
            mod $module;
        )+

        const GROUPED_TEST_FILES: &[&str] = &[$(concat!(stringify!($module), ".rs")),+];
    };
}

grouped_tests!(
    assets_e2e => "../assets_e2e.rs",
    builtin_asset_contract => "../builtin_asset_contract.rs",
    extension_e2e => "../extension_e2e.rs",
    file_e2e => "../file_e2e.rs",
    office_e2e => "../office_e2e.rs",
    shell_e2e => "../shell_e2e.rs",
    workshop_public_e2e => "../workshop_public_e2e.rs",
);

#[test]
fn every_top_level_integration_test_is_registered() {
    use std::collections::BTreeSet;
    use std::path::Path;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: toml::Value = toml::from_str(include_str!("../../Cargo.toml"))
        .expect("nomifun-app Cargo.toml must be valid TOML");
    let mut registered = manifest
        .get("test")
        .and_then(toml::Value::as_array)
        .expect("nomifun-app must declare integration test targets")
        .iter()
        .filter_map(|target| target.get("path").and_then(toml::Value::as_str))
        .filter_map(|path| path.strip_prefix("tests/"))
        .filter(|path| !path.contains('/'))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    registered.extend(GROUPED_TEST_FILES.iter().map(|file| (*file).to_owned()));

    let actual = std::fs::read_dir(manifest_dir.join("tests"))
        .expect("tests directory must be readable")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".rs"))
        .collect::<BTreeSet<_>>();

    assert_eq!(registered, actual, "update Cargo.toml or the content suite when adding an integration test");
}
