//! Pure helpers that compute the immutable `conversation.extra.skills`
//! snapshot for a newly created conversation. No read-time normalization or
//! persistence repair belongs here.

/// Compute the initial `skills` snapshot for a brand-new conversation.
///
/// Formula: `(auto_inject − exclude_auto_inject) ∪ preset_enabled`,
/// sorted ascending, deduplicated.
pub fn compute_initial_skills(
    auto_inject: &[String],
    preset_enabled: &[String],
    exclude_auto_inject: &[String],
) -> Vec<String> {
    let excluded: std::collections::HashSet<&String> = exclude_auto_inject.iter().collect();
    let mut out: std::collections::BTreeSet<String> =
        auto_inject.iter().filter(|n| !excluded.contains(n)).cloned().collect();
    for name in preset_enabled {
        out.insert(name.clone());
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_initial_union_dedup_sort() {
        let skills = compute_initial_skills(
            &["cron".into(), "todo-tracker".into()],
            &["pdf".into(), "cron".into()],
            &[],
        );
        assert_eq!(skills, vec!["cron", "pdf", "todo-tracker"]);
    }

    #[test]
    fn compute_initial_applies_exclude() {
        let skills = compute_initial_skills(&["cron".into(), "todo-tracker".into()], &[], &["cron".into()]);
        assert_eq!(skills, vec!["todo-tracker"]);
    }

    #[test]
    fn compute_initial_exclude_does_not_affect_preset_opt_in() {
        // User excluded cron from auto-inject, but the preset still added it
        // explicitly — preset wins.
        let skills = compute_initial_skills(&["cron".into()], &["cron".into()], &["cron".into()]);
        assert_eq!(skills, vec!["cron"]);
    }

    #[test]
    fn compute_initial_deduplicates_each_input_and_ignores_unknown_exclusions() {
        let skills = compute_initial_skills(
            &["cron".into(), "cron".into(), "todo-tracker".into()],
            &["pdf".into(), "pdf".into()],
            &["missing".into()],
        );
        assert_eq!(skills, vec!["cron", "pdf", "todo-tracker"]);
    }
}
