use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

fn asset_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn builtin_assistants_root() -> PathBuf {
    asset_root().join("builtin-assistants")
}

fn builtin_skills_root() -> PathBuf {
    asset_root().join("builtin-skills")
}

fn read_to_string(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path.as_ref())
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.as_ref().display()))
}

fn collect_markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_markdown_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

#[test]
fn assistant_asset_templates_have_all_supported_locale_files() {
    let manifest: Value =
        serde_json::from_str(&read_to_string(builtin_assistants_root().join("assistants.json"))).unwrap();
    let assistants = manifest["assistants"]
        .as_array()
        .expect("assistants.json must contain assistants array");

    for assistant in assistants {
        for field in ["rule_file", "skill_file"] {
            let Some(template) = assistant[field].as_str() else {
                continue;
            };
            if !template.contains("{locale}") {
                continue;
            }
            for locale in ["en-US", "zh-CN", "ru-RU"] {
                let path = builtin_assistants_root().join(template.replace("{locale}", locale));
                assert!(
                    path.is_file(),
                    "assistant {} declares {field}={template}, but {} is missing",
                    assistant["id"],
                    path.display()
                );
            }
        }
    }
}

#[test]
fn assistant_markdown_does_not_reference_non_materialized_helper_paths() {
    let mut assistant_markdown_files = Vec::new();
    collect_markdown_files(
        &builtin_assistants_root().join("rules"),
        &mut assistant_markdown_files,
    );
    collect_markdown_files(
        &builtin_assistants_root().join("skills"),
        &mut assistant_markdown_files,
    );

    for path in assistant_markdown_files {
        let content = read_to_string(&path);
        assert!(
            !content.contains("assistant/"),
            "{} references the non-materialized assistant resource tree",
            path.display()
        );
        for old_office_path in ["skills/pptx", "skills/docx", "skills/xlsx"] {
            assert!(
                !content.contains(old_office_path),
                "{} references old Office helper path {old_office_path}",
                path.display()
            );
        }
    }
}

#[test]
fn ui_ux_pro_max_assistant_points_to_materializable_skill_assets() {
    let manifest_path = builtin_assistants_root().join("assistants.json");
    let manifest: Value = serde_json::from_str(&read_to_string(&manifest_path)).unwrap();
    let assistants = manifest["assistants"]
        .as_array()
        .expect("assistants.json must contain assistants array");
    let assistant = assistants
        .iter()
        .find(|assistant| assistant["id"] == "ui-ux-pro-max")
        .expect("ui-ux-pro-max assistant must be present");

    let enabled_skills: HashSet<_> = assistant["enabled_skills"]
        .as_array()
        .expect("ui-ux-pro-max enabled_skills must be an array")
        .iter()
        .map(|skill| {
            skill
                .as_str()
                .expect("enabled skill names must be strings")
                .to_owned()
        })
        .collect();
    assert!(
        enabled_skills.contains("ui-ux-pro-max"),
        "ui-ux-pro-max assistant must enable its bundled skill"
    );

    for locale in ["en-US", "zh-CN", "ru-RU"] {
        let rule_path = builtin_assistants_root()
            .join("rules")
            .join(format!("ui-ux-pro-max.{locale}.md"));
        let rule = read_to_string(&rule_path);
        assert!(
            !rule.contains("assistant/ui-ux-pro-max"),
            "{} still points to the non-materialized assistant resource tree",
            rule_path.display()
        );
        assert!(
            rule.contains(".nomi/skills/ui-ux-pro-max/scripts/search.py"),
            "{} must point agents at the materialized builtin skill script",
            rule_path.display()
        );
    }

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
fn morph_ppt_style_library_references_match_packaged_assets() {
    let morph_ppt_root = builtin_skills_root().join("morph-ppt");
    let index_path = morph_ppt_root.join("reference/styles/INDEX.md");
    let index = read_to_string(&index_path);

    for line in index.lines().filter(|line| line.starts_with("| ")) {
        if line.contains("Directory") || line.contains("---") {
            continue;
        }
        let cells: Vec<_> = line
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim())
            .collect();
        let Some(style_id) = cells.first().copied() else {
            continue;
        };
        if style_id.is_empty() || !style_id.contains("--") {
            continue;
        }
        let style_dir = morph_ppt_root.join("reference/styles").join(style_id);
        assert!(
            style_dir.join("style.md").is_file(),
            "{} lists {style_id}, but style.md is missing",
            index_path.display()
        );
    }

    let mut style_files = Vec::new();
    collect_markdown_files(&morph_ppt_root.join("reference/styles"), &mut style_files);
    for path in style_files
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("style.md"))
    {
        let content = read_to_string(&path);
        for script in ["build.sh", "build.py"] {
            if content.contains(script) {
                assert!(
                    path.parent().unwrap().join(script).is_file(),
                    "{} references missing {script}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn morph_ppt_3d_points_to_real_style_index() {
    let skill_path = builtin_skills_root().join("morph-ppt-3d/SKILL.md");
    let skill = read_to_string(&skill_path);
    assert!(
        !skill.contains("../../styles/INDEX.md"),
        "{} points to a non-existent styles index",
        skill_path.display()
    );
    assert!(
        builtin_skills_root()
            .join("morph-ppt/reference/styles/INDEX.md")
            .is_file(),
        "morph-ppt style index must be packaged"
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
