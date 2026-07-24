use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

fn asset_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn builtin_presets_root() -> PathBuf {
    asset_root().join("builtin-presets")
}

fn builtin_skills_root() -> PathBuf {
    asset_root().join("builtin-skills")
}

fn read_to_string(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path.as_ref())
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.as_ref().display()))
}

#[test]
fn preset_asset_templates_have_all_supported_locale_files() {
    let manifest: Value =
        serde_json::from_str(&read_to_string(builtin_presets_root().join("presets.json"))).unwrap();
    let presets = manifest["presets"]
        .as_array()
        .expect("presets.json must contain presets array");

    for preset in presets {
        for field in ["rule_file"] {
            let Some(template) = preset[field].as_str() else {
                continue;
            };
            if !template.contains("{locale}") {
                continue;
            }
            for locale in ["en-US", "zh-CN", "ru-RU"] {
                let path = builtin_presets_root().join(template.replace("{locale}", locale));
                assert!(
                    path.is_file(),
                    "preset {} declares {field}={template}, but {} is missing",
                    preset["id"],
                    path.display()
                );
            }
        }
    }
}

#[test]
fn ui_ux_pro_max_is_a_self_contained_skill() {
    let skill_root = builtin_skills_root().join("ui-ux-pro-max");
    assert!(
        skill_root.join("SKILL.md").is_file(),
        "ui-ux-pro-max skill must include SKILL.md"
    );
    assert!(
        skill_root.join("scripts/search.py").is_file(),
        "ui-ux-pro-max skill must include scripts/search.py"
    );
    assert!(
        skill_root.join("data/catalog.json").is_file(),
        "ui-ux-pro-max skill must include searchable data/catalog.json"
    );
}

#[test]
fn migrated_workflows_are_self_contained_skills_with_display_metadata() {
    let metadata: Value =
        serde_json::from_str(&read_to_string(builtin_skills_root().join("skill-tags.json"))).unwrap();
    let entries = metadata["skills"]
        .as_array()
        .expect("skill-tags.json must contain a skills array");

    for name in ["planning-with-files"] {
        let skill_path = builtin_skills_root().join(name).join("SKILL.md");
        let skill = read_to_string(&skill_path);
        assert!(skill.starts_with("---\n"), "{} must start with YAML frontmatter", skill_path.display());
        assert!(skill.contains(&format!("name: {name}")), "{} must declare its folder name", skill_path.display());
        assert!(
            entries.iter().any(|entry| entry["name"] == name),
            "{name} must have localized display metadata"
        );
    }
}

#[test]
fn builtin_skill_display_metadata_matches_the_packaged_corpus() {
    let metadata: Value =
        serde_json::from_str(&read_to_string(builtin_skills_root().join("skill-tags.json"))).unwrap();
    let metadata_names: HashSet<String> = metadata["skills"]
        .as_array()
        .expect("skill-tags.json must contain a skills array")
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .expect("every display metadata entry must have a name")
                .to_owned()
        })
        .collect();

    let root = builtin_skills_root();
    let mut packaged_names = HashSet::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("auto-inject") {
            for child in std::fs::read_dir(&path).unwrap() {
                let child = child.unwrap().path();
                if child.join("SKILL.md").is_file() {
                    packaged_names.insert(child.file_name().unwrap().to_string_lossy().into_owned());
                }
            }
        } else if path.join("SKILL.md").is_file() {
            packaged_names.insert(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    assert_eq!(
        metadata_names, packaged_names,
        "every packaged builtin Skill must have exactly one display metadata entry"
    );
}

#[tokio::test]
async fn ui_ux_pro_max_skill_materializes_from_embedded_builtin_corpus() {
    let tmp = TempDir::new().unwrap();
    let wrote = nomifun_extension::materialize_if_needed(
        tmp.path(),
        nomifun_extension::builtin_skills_corpus(),
        "asset-contract-test",
    )
    .await
    .unwrap();

    assert!(wrote, "empty data dir should trigger materialization");
    let materialized = tmp.path().join("builtin-skills").join("ui-ux-pro-max");
    assert!(materialized.join("SKILL.md").is_file());
    assert!(materialized.join("scripts/search.py").is_file());
    assert!(materialized.join("data/catalog.json").is_file());
}
