//! Shared knowledge-context builder — the single source of truth for the
//! prompt/document text that tells an agent which knowledge bases are
//! mounted, how to retrieve from them, and which write-back contract
//! applies.
//!
//! Consumers:
//! - `nomifun-ai-agent` factory paths (ACP assembler preset context, nomi
//!   engine system prompt) via [`KnowledgeContextFormat::PromptSection`];
//! - the terminal-session task (C1) writes a standalone
//!   `{cwd}/.nomi/knowledge/README.md` via
//!   [`KnowledgeContextFormat::TerminalReadme`].
//!
//! All agent-facing contract wording is English by project convention.

use nomifun_api_types::KnowledgeMountInfo;

/// Per-base cap on TOC file lines injected into the context (bounds token
/// cost while keeping enough navigation surface for hit rate).
pub const TOC_PER_KB_MAX: usize = 20;

/// Global cap on TOC file lines across all mounted bases. When many bases
/// are mounted the per-base budget shrinks to `TOC_GLOBAL_MAX / n`.
pub const TOC_GLOBAL_MAX: usize = 60;

/// Typed write-back mode, parsed ONCE at the context-builder entry point.
/// The wire/API surfaces keep passing strings
/// ([`KnowledgeContextOptions::writeback_mode`] stays `Option<&str>`); this
/// enum replaces the internal string comparisons so a typo'd mode can never
/// silently pick a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritebackMode {
    /// Agent writes are confined to `_inbox/{target_id}/` (the safe default).
    #[default]
    Staged,
    /// The agent may edit the base body directly.
    Direct,
}

impl WritebackMode {
    /// Parse a wire string: `None`/`"staged"` → [`Self::Staged`], `"direct"`
    /// → [`Self::Direct`]. Unknown values fall back to the safe default
    /// ([`Self::Staged`]) with a warning — never to the more permissive mode.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            None | Some("staged") => Self::Staged,
            Some("direct") => Self::Direct,
            Some(other) => {
                tracing::warn!(writeback_mode = other, "unknown writeback_mode; falling back to staged");
                Self::Staged
            }
        }
    }
}

/// Typed write-back disposition ("回写意识"), parsed ONCE at the
/// context-builder entry point. ORTHOGONAL to [`WritebackMode`]: the mode
/// decides WHERE writes land (staged inbox vs direct body), the eagerness
/// decides HOW EAGERLY the agent writes at all. The wire/API surfaces keep
/// passing strings ([`KnowledgeContextOptions::writeback_eagerness`] stays
/// `Option<&str>`); this enum replaces internal string comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritebackEagerness {
    /// Restrained: only persist knowledge the model judges clearly worth
    /// keeping. The historical behaviour and the safe default.
    #[default]
    Conservative,
    /// Bold: capture anything plausibly relevant to a mounted base without
    /// much hesitation; the user prunes later.
    Aggressive,
}

impl WritebackEagerness {
    /// Parse a wire string: `None`/`"conservative"` → [`Self::Conservative`],
    /// `"aggressive"` → [`Self::Aggressive`]. Unknown values fall back to the
    /// restrained default ([`Self::Conservative`]) with a warning — never to
    /// the more eager mode.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            None | Some("conservative") => Self::Conservative,
            Some("aggressive") => Self::Aggressive,
            Some(other) => {
                tracing::warn!(
                    writeback_eagerness = other,
                    "unknown writeback_eagerness; falling back to conservative"
                );
                Self::Conservative
            }
        }
    }
}

/// Output shape of [`build_knowledge_context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeContextFormat {
    /// A `## Knowledge bases (extended knowledge source)` section meant to be
    /// embedded into a larger system prompt / preset context.
    PromptSection,
    /// A standalone, complete markdown document (H1 + intro) meant to be
    /// written as `README.md` inside the workspace mount directory.
    TerminalReadme,
}

/// Inputs beyond the mounts themselves. `target_id` is the session-scoped
/// identifier (conversation id today, terminal id for C1) used to scope the
/// staged write-back inbox path `_inbox/{target_id}/`.
#[derive(Debug, Clone)]
pub struct KnowledgeContextOptions<'a> {
    pub format: KnowledgeContextFormat,
    /// Write-back ("回血") switch — `false` renders the read-only contract.
    pub writeback: bool,
    /// `staged` (default when `None`) or `direct`; only meaningful while
    /// `writeback` is true.
    pub writeback_mode: Option<&'a str>,
    /// `conservative` (default when `None`) or `aggressive`; the write-back
    /// disposition ("回写意识"), only meaningful while `writeback` is true.
    pub writeback_eagerness: Option<&'a str>,
    /// Conversation / terminal id scoping staged write-backs.
    pub target_id: &'a str,
    /// Whether THIS surface exposes a `knowledge_search` agent tool. When true,
    /// the protocol leads with an imperative to call it; when false (e.g. a raw
    /// terminal PTY, or an ACP session before the knowledge MCP exists), it
    /// keeps the Grep/Read file-navigation wording.
    pub has_search_tool: bool,
    /// Whether THIS surface exposes the native `knowledge_write` agent tool.
    /// When true, the write-back contract tells the agent to CALL it — the
    /// reliable path for nomi-engine sessions, where the generic `Write` tool
    /// has no workspace cwd (relative mount paths miss the base) and sits behind
    /// the approval gate. When false (terminal PTY, ACP file-based sessions),
    /// the contract keeps the file-write prose against the mounted directory.
    pub has_write_tool: bool,
}

/// Render the knowledge context for the given mounts. Returns `None` when
/// nothing is mounted (callers skip the section entirely).
pub fn build_knowledge_context(
    mounts: &[KnowledgeMountInfo],
    options: &KnowledgeContextOptions<'_>,
) -> Option<String> {
    if mounts.is_empty() {
        return None;
    }

    // Exhaustive on purpose: a future format variant must consciously pick
    // its rendering branch instead of silently falling into one of them.
    let readme = match options.format {
        KnowledgeContextFormat::TerminalReadme => true,
        KnowledgeContextFormat::PromptSection => false,
    };
    // Parse the writeback mode once at the entry point; everything below
    // works on the typed value.
    let writeback_mode = WritebackMode::parse(options.writeback_mode);
    let writeback_eagerness = WritebackEagerness::parse(options.writeback_eagerness);
    let mut out = String::new();

    // ── Header ───────────────────────────────────────────────────────
    if readme {
        out.push_str(
            "# Knowledge bases\n\n\
             This directory is mounted and managed by the NomiFun platform. It contains the \
             knowledge bases bound to this session — a curated, extended knowledge source for \
             your work here. Paths below are relative to the workspace root.\n\n\
             ## Retrieval protocol\n\n",
        );
    } else {
        out.push_str(
            "## Knowledge bases (extended knowledge source)\n\
             The following knowledge bases are mounted into this workspace as markdown \
             directories — a curated, extended knowledge source for this session.\n\n\
             Retrieval protocol:\n",
        );
    }

    // ── Retrieval protocol (rendered once, not per base) ─────────────
    if options.has_search_tool {
        out.push_str(
            "1. Search first, then answer: when a task or question touches any topic covered \
             below, call the `knowledge_search` tool BEFORE answering from memory. It searches \
             the real base content directly (so it finds matches even when Grep/Glob cannot) and \
             returns ranked `base / path — heading` results, each with an opaque `handle`.\n\
             2. To read a full document, call the `knowledge_read` tool with its `handle` (no path \
             needed). The per-base tables of contents below are a map for browsing when you already \
             know the structure.\n",
        );
    } else {
        out.push_str(
            "1. Search first, then answer: when a task or question touches any topic covered \
             below, consult the matching knowledge base BEFORE answering from memory.\n\
             2. Locate documents via each base's table of contents, then read the file. For \
             anything not listed, search the base's mount path with Grep/Glob instead of \
             crawling directories blindly.\n",
        );
    }
    out.push_str(
        "3. A line like `docs/ — 12 files` summarizes a folder too large to list in full; \
         explore that folder directly when it looks relevant.\n\
         4. When you cite knowledge in an answer, reference the source file by its relative \
         path inside the mount.\n\
         5. ",
    );
    out.push_str(&writeback_contract(options, writeback_mode, writeback_eagerness));
    out.push('\n');

    // ── Per-base sections ─────────────────────────────────────────────
    if readme {
        out.push_str("\n## Mounted bases\n");
    }
    for m in mounts {
        out.push_str(&format!("\n### {}\n", m.name));
        out.push_str(&format!("- Path: `./{}/`\n", m.rel_path));
        let description = m.description.trim();
        if !description.is_empty() {
            out.push_str(&format!("- Description: {description}\n"));
        }
        if let Some(summary) = m.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("- Summary: {summary}\n"));
        }
        let has_summary = m.summary.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
        if description.is_empty() && !has_summary {
            let hints = toc_topic_hints(&m.toc);
            if !hints.is_empty() {
                out.push_str(&format!("- Topics include: {hints}\n"));
            }
        }
        out.push_str(&format!(
            "- When to consult: any task or question related to \"{}\" or the topics above — \
             read the matching documents first.\n",
            m.name
        ));
        if !m.toc.is_empty() {
            out.push_str("- Contents:\n");
            for entry in &m.toc {
                out.push_str(&format!("  - {entry}\n"));
            }
        }
    }

    // ── Realtime (live URL) sources ───────────────────────────────────
    if mounts.iter().any(|m| !m.live_sources.is_empty()) {
        out.push_str(if readme { "\n## Realtime sources\n" } else { "\n### Realtime sources\n" });
        out.push_str(
            "Some bases are backed by live URL sources; their mounted snapshots may be stale. \
             When freshness matters, fetch the URL directly:\n",
        );
        for m in mounts {
            for src in &m.live_sources {
                match src.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                    Some(title) => out.push_str(&format!("- {title} — {} (base: \"{}\")\n", src.url, m.name)),
                    None => out.push_str(&format!("- {} (base: \"{}\")\n", src.url, m.name)),
                }
            }
        }
        // Layered tool guidance: `nomi_knowledge_fetch_url` is a desktop
        // gateway tool — terminal CLI sessions and plain chat sessions never
        // have it, so the text must not promise it unconditionally.
        out.push_str(
            "To read one of these URLs at its current state, use a web-fetch tool already \
             available in this session if you have one; otherwise, if a \
             `nomi_knowledge_fetch_url` tool is available, use that. If neither is \
             available, do not improvise: tell the user that this session cannot read \
             realtime sources, and answer from the mounted snapshots while noting they \
             may be stale.\n",
        );
    }

    Some(out)
}

/// Up to 6 document headings pulled from a base's budgeted TOC, joined with
/// "; ", for the "Topics include" hint shown when a base has no
/// description/summary. Skips aggregate rows (`dir/ — N files`) and the
/// `(+N more files)` remainder so only real document titles become hints.
fn toc_topic_hints(toc: &[String]) -> String {
    toc.iter()
        .filter_map(|line| line.split_once(" — ").map(|(_, title)| title.trim()))
        .filter(|t| !t.is_empty() && !t.ends_with(" files") && *t != "files")
        .take(6)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The write-back ("回血") contract paragraph. Wording is load-bearing:
/// staged mode confines writes to `_inbox/{target_id}/`, direct mode allows
/// editing the base body, disabled declares everything read-only. When
/// write-back is enabled, the disposition (`eagerness`) sentence is appended
/// to tune HOW EAGERLY the agent writes — orthogonal to the staged/direct
/// placement decision.
fn writeback_contract(
    options: &KnowledgeContextOptions<'_>,
    mode: WritebackMode,
    eagerness: WritebackEagerness,
) -> String {
    if !options.writeback {
        return "Write-back is DISABLED for this session: treat these directories as READ-ONLY. \
                Do not create, modify, or delete any files inside them."
            .to_owned();
    }
    // Tool-based contract: the surface exposes the native `knowledge_write`
    // tool, so instruct the agent to CALL it. This is the reliable path for the
    // nomi engine — the tool resolves the base + placement internally and is
    // allow-listed past the approval gate, unlike the generic Write tool.
    let mut contract = if options.has_write_tool {
        match mode {
            WritebackMode::Staged => "Write-back is ENABLED in STAGED mode: when you produce reusable knowledge \
                 (conclusions, domain facts, lessons learned), persist it by CALLING the `knowledge_write` tool. \
                 To UPDATE an existing document, pass the `handle` from a `knowledge_search` result (read it first \
                 with `knowledge_read`, merge, then write the full `content`); to CREATE a new one, pass `base` plus \
                 a descriptive `.md` `rel_path`. The system automatically places your write in a review inbox keyed \
                 to this session — you do NOT manage the path, and the original document is left untouched for the \
                 user to merge later. Never rebuild paths by hand. Do NOT use the generic Write/Edit file tools; \
                 treat the mounted base files as READ-ONLY."
                .to_owned(),
            WritebackMode::Direct => "Write-back is ENABLED in DIRECT mode: when you produce reusable knowledge \
                 (conclusions, domain facts, lessons learned), persist it by CALLING the `knowledge_write` tool — it \
                 writes straight into the matching knowledge base. To UPDATE an existing document, pass the `handle` \
                 from a `knowledge_search` result (read it first with `knowledge_read`, merge, then write the full \
                 `content`); to CREATE a new one, pass `base` plus a descriptive `.md` `rel_path`. Never rebuild \
                 paths by hand. Do NOT use the generic Write/Edit file tools for knowledge; never delete files."
                .to_owned(),
        }
    } else {
        match mode {
            WritebackMode::Staged => format!(
                "Write-back is ENABLED in STAGED mode: when you produce reusable knowledge \
                 (conclusions, domain facts, lessons learned), distill it into well-structured \
                 markdown files and save them ONLY under `_inbox/{}/` inside the matching \
                 knowledge base directory (create it if missing). Treat everything else in the \
                 knowledge bases as READ-ONLY — never modify or delete existing documents. \
                 Staged notes are reviewed and merged by the user later, so make each file \
                 self-contained, concise, and free of session-specific noise.",
                options.target_id
            ),
            WritebackMode::Direct => {
                "Write-back is ENABLED in DIRECT mode: when you produce reusable knowledge \
                 (conclusions, domain facts, lessons learned), distill it into well-structured \
                 markdown files inside the matching knowledge base directory — create new files or \
                 make small, focused updates to existing ones. Never rewrite documents wholesale \
                 and never delete files; other sessions may be using the same base concurrently. \
                 Keep entries concise, organized, and free of session-specific noise."
                    .to_owned()
            }
        }
    };
    contract.push(' ');
    contract.push_str(eagerness_clause(eagerness));
    contract
}

/// The write-back disposition ("回写意识") sentence appended to an enabled
/// write-back contract. Only the threshold for WHAT to write changes — the
/// placement rules (staged/direct) above are unaffected.
fn eagerness_clause(eagerness: WritebackEagerness) -> &'static str {
    match eagerness {
        WritebackEagerness::Conservative => {
            "Disposition — CONSERVATIVE: be restrained about what you write back. Persist only \
             durable, broadly reusable knowledge you judge clearly worth keeping; when in doubt, \
             do NOT write. Skip session-specific, trivial, redundant, or uncertain material."
        }
        WritebackEagerness::Aggressive => {
            "Disposition — AGGRESSIVE: be eager to write back. Whenever you encounter or produce \
             anything plausibly relevant to a mounted base — facts, decisions, useful snippets, \
             observations, gotchas — capture it without much hesitation, even if you are unsure it \
             will be reused. Prefer over-capturing to losing knowledge; the user prunes later. \
             Still skip secrets and pure session noise, and keep each entry self-contained."
        }
    }
}

/// Max aggregated `dir/ — N files` rows appended per base; overflow beyond
/// the largest directories folds into the `(+N more files)` row.
pub const TOC_AGGREGATE_DIR_MAX: usize = 8;

/// Apply the per-KB / global TOC budgets to full per-base file listings
/// (one inner `Vec` per mounted base, lines sorted by path).
///
/// The budget caps **file lines only**: a base keeps at most
/// `min(TOC_PER_KB_MAX, TOC_GLOBAL_MAX / n_bases)` individual file rows, so
/// file lines never exceed `TOC_GLOBAL_MAX` in total. Overflowing files are
/// then summarized in ADDITIONAL aggregate rows — up to
/// [`TOC_AGGREGATE_DIR_MAX`] `dir/ — N files` rows (the top-level
/// directories with the most overflow, rendered in path order) plus one
/// `(+N more files)` row counting root-level overflow and any directories
/// beyond the top-8. A budgeted TOC may therefore be up to 9 rows longer
/// than its file-line budget.
pub fn apply_toc_budgets(tocs: &mut [Vec<String>]) {
    if tocs.is_empty() {
        return;
    }
    let per_kb = TOC_PER_KB_MAX.min((TOC_GLOBAL_MAX / tocs.len()).max(1));
    for toc in tocs.iter_mut() {
        if toc.len() <= per_kb {
            continue;
        }
        let overflow = toc.split_off(per_kb);
        let mut dirs: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        let mut rest = 0usize;
        for line in &overflow {
            // The path is everything before the ` — title` suffix (if any).
            let path = line.split(" — ").next().unwrap_or(line);
            match path.split_once('/') {
                Some((dir, _)) => *dirs.entry(dir).or_default() += 1,
                None => rest += 1,
            }
        }
        // Keep only the heaviest directories as named rows; everything else
        // joins the `(+N more files)` remainder.
        let mut dir_counts: Vec<(&str, usize)> = dirs.into_iter().collect();
        if dir_counts.len() > TOC_AGGREGATE_DIR_MAX {
            dir_counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            for (_, n) in dir_counts.split_off(TOC_AGGREGATE_DIR_MAX) {
                rest += n;
            }
            dir_counts.sort_by(|a, b| a.0.cmp(b.0));
        }
        toc.extend(dir_counts.into_iter().map(|(dir, n)| format!("{dir}/ — {n} files")));
        if rest > 0 {
            toc.push(format!("(+{rest} more files)"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::KnowledgeSourceEntry;

    fn mount(name: &str, rel: &str) -> KnowledgeMountInfo {
        KnowledgeMountInfo {
            knowledge_base_id: nomifun_common::KnowledgeBaseId::new(),
            name: name.to_owned(),
            description: String::new(),
            rel_path: rel.to_owned(),
            toc: Vec::new(),
            summary: None,
            live_sources: Vec::new(),
        }
    }

    fn prompt_opts<'a>(writeback: bool, mode: Option<&'a str>, target: &'a str) -> KnowledgeContextOptions<'a> {
        KnowledgeContextOptions {
            format: KnowledgeContextFormat::PromptSection,
            writeback,
            writeback_mode: mode,
            writeback_eagerness: None,
            target_id: target,
            has_search_tool: false,
            has_write_tool: false,
        }
    }

    /// Like [`prompt_opts`] but lets a test pin the disposition explicitly.
    fn prompt_opts_eager<'a>(
        writeback: bool,
        mode: Option<&'a str>,
        eagerness: Option<&'a str>,
        target: &'a str,
    ) -> KnowledgeContextOptions<'a> {
        KnowledgeContextOptions {
            format: KnowledgeContextFormat::PromptSection,
            writeback,
            writeback_mode: mode,
            writeback_eagerness: eagerness,
            target_id: target,
            has_search_tool: false,
            has_write_tool: false,
        }
    }

    // ── empty input ──────────────────────────────────────────────────

    #[test]
    fn empty_mounts_build_nothing() {
        assert_eq!(
            build_knowledge_context(
                &[],
                &prompt_opts(
                    false,
                    None,
                    "0190f5fe-7c00-7a00-8000-000000000001",
                ),
            ),
            None
        );
        let readme_opts = KnowledgeContextOptions {
            format: KnowledgeContextFormat::TerminalReadme,
            writeback: true,
            writeback_mode: None,
            writeback_eagerness: None,
            target_id: "0190f5fe-7c00-7a00-8000-000000000001",
            has_search_tool: false,
            has_write_tool: false,
        };
        assert_eq!(build_knowledge_context(&[], &readme_opts), None);
    }

    // ── single base: full per-base contract ──────────────────────────

    #[test]
    fn single_base_prompt_section_contract() {
        let mut m = mount("领域知识", ".nomi/knowledge/领域知识");
        m.description = "团队约定".into();
        m.summary = Some("Covers deployment flows and on-call runbooks.".into());
        m.toc = vec!["concepts/术语.md — 术语表".into(), "(+3 more files)".into()];

        let out = build_knowledge_context(
            &[m],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();

        // Section heading stays compatible with the historical one.
        assert!(out.starts_with("## Knowledge bases (extended knowledge source)"), "got: {out}");
        // Retrieval protocol: search-before-answer, Grep/Read guidance,
        // relative-path citation, abridged-TOC explore hint — rendered once.
        assert!(out.contains("Retrieval protocol"), "got: {out}");
        assert!(out.contains("BEFORE answering"), "got: {out}");
        assert!(out.contains("Grep"), "got: {out}");
        assert!(out.contains("relative path"), "got: {out}");
        assert!(out.contains("summarizes a folder"), "got: {out}");
        // Per-base section: name, path, description, summary, when-to-consult, TOC.
        assert!(out.contains("### 领域知识"), "got: {out}");
        assert!(out.contains("`./.nomi/knowledge/领域知识/`"), "got: {out}");
        assert!(out.contains("团队约定"), "got: {out}");
        assert!(out.contains("Covers deployment flows and on-call runbooks."), "got: {out}");
        assert!(out.contains("When to consult"), "got: {out}");
        assert!(out.contains("concepts/术语.md — 术语表"), "got: {out}");
        assert!(out.contains("(+3 more files)"), "got: {out}");
        // Read-only contract when write-back is off.
        assert!(out.contains("Write-back is DISABLED"), "got: {out}");
        assert!(out.contains("READ-ONLY"), "got: {out}");
        // No live sources → no realtime section, no fetch-tool mention.
        assert!(!out.contains("Realtime sources"), "got: {out}");
        assert!(!out.contains("nomi_knowledge_fetch_url"), "got: {out}");
    }

    #[test]
    fn empty_description_and_summary_lines_are_omitted() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let out = build_knowledge_context(
            &[m],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(!out.contains("Description:"), "got: {out}");
        assert!(!out.contains("Summary:"), "got: {out}");
        // The when-to-consult guidance survives even without description.
        assert!(out.contains("When to consult"), "got: {out}");
    }

    #[test]
    fn empty_description_base_surfaces_topic_hints_from_toc() {
        let mut m = mount("库A", ".nomi/knowledge/库A");
        m.toc = vec![
            "deploy/rollback.md — 回滚流程".into(),
            "concepts/术语.md — 术语表".into(),
            "docs/ — 12 files".into(),
            "(+3 more files)".into(),
        ];
        let out = build_knowledge_context(
            &[m],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(out.contains("Topics include:"), "got: {out}");
        assert!(out.contains("回滚流程"), "got: {out}");
        assert!(out.contains("术语表"), "got: {out}");
        // Aggregate rows (`dir/ — N files`) and the `(+N more)` remainder must
        // not be promoted to hints. The verbatim TOC still lists them under
        // `- Contents:` and the protocol preamble names one as an example, so
        // scope the check to the `Topics include:` line itself.
        let hint_line = out
            .lines()
            .find(|l| l.starts_with("- Topics include:"))
            .expect("hint line present");
        assert!(!hint_line.contains("12 files"), "aggregate rows must not be hints: {hint_line}");
    }

    #[test]
    fn described_base_omits_topic_hints() {
        let mut m = mount("库A", ".nomi/knowledge/库A");
        m.description = "团队约定".into();
        m.toc = vec!["x.md — 标题".into()];
        let out = build_knowledge_context(
            &[m],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(!out.contains("Topics include:"), "described base needs no hint line: {out}");
        assert!(out.contains("团队约定"));
    }

    // ── multi base: protocol rendered once ───────────────────────────

    #[test]
    fn multi_base_renders_protocol_once() {
        let a = mount("库A", ".nomi/knowledge/库A");
        let b = mount("库B", ".nomi/knowledge/库B");
        let out = build_knowledge_context(
            &[a, b],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert_eq!(out.matches("Retrieval protocol").count(), 1, "got: {out}");
        assert_eq!(out.matches("Write-back is DISABLED").count(), 1, "got: {out}");
        assert!(out.contains("### 库A"), "got: {out}");
        assert!(out.contains("### 库B"), "got: {out}");
    }

    // ── tool-aware retrieval protocol ────────────────────────────────

    #[test]
    fn protocol_mentions_search_tool_when_available() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let mut opts = prompt_opts(
            false,
            None,
            "0190f5fe-7c00-7a00-8000-000000000001",
        );
        opts.has_search_tool = true;
        let out = build_knowledge_context(std::slice::from_ref(&m), &opts).unwrap();
        assert!(out.contains("call the `knowledge_search` tool"), "got: {out}");
        assert!(!out.contains("Grep/Glob instead of"), "search variant drops Grep-first wording: {out}");
    }

    #[test]
    fn protocol_keeps_grep_wording_without_search_tool() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let out = build_knowledge_context(
            &[m],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(out.contains("Grep/Glob"), "got: {out}");
        assert!(!out.contains("knowledge_search"), "got: {out}");
    }

    // ── TOC budgets ──────────────────────────────────────────────────

    #[test]
    fn toc_budget_keeps_small_listings_untouched() {
        let mut tocs = vec![vec!["a.md".to_string(), "b.md — B".to_string()]];
        apply_toc_budgets(&mut tocs);
        assert_eq!(tocs[0], vec!["a.md".to_string(), "b.md — B".to_string()]);
    }

    #[test]
    fn toc_budget_aggregates_per_kb_overflow_by_directory() {
        // 10 root files + 15 under docs/ = 25 sorted lines; per-KB budget 20
        // → keep first 20, aggregate the 5 overflowing docs/ files.
        let mut lines: Vec<String> = (0..10).map(|i| format!("a{i:02}.md — Root {i}")).collect();
        lines.extend((0..15).map(|i| format!("docs/d{i:02}.md — Doc {i}")));
        let mut tocs = vec![lines];
        apply_toc_budgets(&mut tocs);

        let toc = &tocs[0];
        assert_eq!(toc.len(), TOC_PER_KB_MAX + 1, "got: {toc:?}");
        assert_eq!(toc[0], "a00.md — Root 0");
        assert_eq!(toc[TOC_PER_KB_MAX - 1], "docs/d09.md — Doc 9");
        assert_eq!(toc[TOC_PER_KB_MAX], "docs/ — 5 files");
    }

    #[test]
    fn toc_budget_rootless_overflow_keeps_more_files_marker() {
        let mut tocs = vec![(0..25).map(|i| format!("f{i:02}.md")).collect::<Vec<_>>()];
        apply_toc_budgets(&mut tocs);
        let toc = &tocs[0];
        assert_eq!(toc.len(), TOC_PER_KB_MAX + 1, "got: {toc:?}");
        assert_eq!(toc[TOC_PER_KB_MAX], "(+5 more files)");
    }

    /// Many distinct overflowing directories must not balloon the TOC: only
    /// the top-[`TOC_AGGREGATE_DIR_MAX`] directories get named rows, the
    /// rest (plus rootless overflow) folds into `(+N more files)`.
    #[test]
    fn toc_budget_caps_aggregate_directory_rows() {
        // Budget-filling root files first, then 10 dirs with growing file
        // counts (d0: 1 file … d9: 10 files) plus 3 rootless files.
        let mut lines: Vec<String> = (0..TOC_PER_KB_MAX).map(|i| format!("a{i:02}.md")).collect();
        for d in 0..10 {
            for f in 0..=d {
                lines.push(format!("d{d}/f{f:02}.md"));
            }
        }
        lines.extend((0..3).map(|i| format!("z{i}.md")));
        let mut tocs = vec![lines];
        apply_toc_budgets(&mut tocs);

        let toc = &tocs[0];
        let dir_rows: Vec<&String> = toc.iter().filter(|l| l.contains("/ — ")).collect();
        assert_eq!(dir_rows.len(), TOC_AGGREGATE_DIR_MAX, "got: {toc:?}");
        // The two SMALLEST dirs (d0: 1, d1: 2) are folded, the rest named.
        assert!(!toc.iter().any(|l| l.starts_with("d0/")), "got: {toc:?}");
        assert!(!toc.iter().any(|l| l.starts_with("d1/")), "got: {toc:?}");
        assert!(toc.contains(&"d9/ — 10 files".to_string()), "got: {toc:?}");
        // Named rows stay in path order.
        assert_eq!(dir_rows[0], "d2/ — 3 files", "got: {toc:?}");
        // Remainder: d0(1) + d1(2) dirs + 3 rootless = 6.
        assert_eq!(toc.last().unwrap(), "(+6 more files)", "got: {toc:?}");
        // Total rows = budget + 8 dir rows + 1 remainder row.
        assert_eq!(toc.len(), TOC_PER_KB_MAX + TOC_AGGREGATE_DIR_MAX + 1, "got: {toc:?}");
    }

    #[test]
    fn toc_budget_shrinks_per_kb_share_under_global_cap() {
        // 4 bases × 20 files = 80 > 60 → fair share 15 file lines each.
        let mut tocs: Vec<Vec<String>> = (0..4)
            .map(|kb| (0..20).map(|i| format!("kb{kb}/f{i:02}.md")).collect())
            .collect();
        apply_toc_budgets(&mut tocs);
        for toc in &tocs {
            let file_lines = toc.iter().filter(|l| l.contains(".md")).count();
            assert_eq!(file_lines, 15, "got: {toc:?}");
            assert!(toc.iter().any(|l| l.ends_with("— 5 files")), "got: {toc:?}");
        }
    }

    // ── write-back contract ──────────────────────────────────────────

    #[test]
    fn staged_writeback_scopes_inbox_to_target_id() {
        let m = mount("库A", ".nomi/knowledge/库A");
        // Default mode (None) is staged.
        let out = build_knowledge_context(
            std::slice::from_ref(&m),
            &prompt_opts(true, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(out.contains("STAGED mode"), "got: {out}");
        assert!(
            out.contains("_inbox/0190f5fe-7c00-7a00-8000-000000000001/"),
            "got: {out}"
        );
        assert!(out.contains("READ-ONLY"), "got: {out}");
        // Explicit "staged" renders identically.
        let explicit = build_knowledge_context(
            &[m],
            &prompt_opts(
                true,
                Some("staged"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert_eq!(out, explicit);
    }

    #[test]
    fn direct_writeback_never_mentions_inbox() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let out = build_knowledge_context(
            &[m],
            &prompt_opts(
                true,
                Some("direct"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(out.contains("DIRECT mode"), "got: {out}");
        assert!(!out.contains("_inbox"), "got: {out}");
    }

    #[test]
    fn tool_writeback_contract_directs_handle_use_for_updates() {
        let m = mount("库A", ".nomi/knowledge/库A");
        // DIRECT + tools available: update via handle, read via knowledge_read.
        let mut direct = prompt_opts(
            true,
            Some("direct"),
            "0190f5fe-7c00-7a00-8000-000000000001",
        );
        direct.has_write_tool = true;
        direct.has_search_tool = true;
        let out = build_knowledge_context(std::slice::from_ref(&m), &direct).unwrap();
        assert!(out.contains("knowledge_write"), "got: {out}");
        assert!(out.contains("handle"), "update path must reference the handle: {out}");
        assert!(out.contains("knowledge_read"), "got: {out}");
        // STAGED + tools: emphasize auto-placement + original untouched.
        let mut staged = prompt_opts(
            true,
            Some("staged"),
            "0190f5fe-7c00-7a00-8000-000000000001",
        );
        staged.has_write_tool = true;
        staged.has_search_tool = true;
        let s = build_knowledge_context(&[m], &staged).unwrap();
        assert!(s.contains("handle") && s.contains("review inbox"), "got: {s}");
        assert!(s.contains("left untouched"), "got: {s}");
    }

    // ── realtime (live URL) sources ──────────────────────────────────

    #[test]
    fn live_sources_render_realtime_section() {
        let mut m = mount("接口库", ".nomi/knowledge/接口库");
        m.live_sources = vec![
            KnowledgeSourceEntry {
                url: "https://example.com/api-docs".into(),
                title: Some("API docs".into()),
                rendered: false,
            },
            KnowledgeSourceEntry {
                url: "https://example.com/changelog".into(),
                title: None,
                rendered: false,
            },
        ];
        let plain = mount("普通库", ".nomi/knowledge/普通库");

        let out = build_knowledge_context(
            &[m, plain],
            &prompt_opts(false, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(out.contains("Realtime sources"), "got: {out}");
        assert!(out.contains("API docs"), "got: {out}");
        assert!(out.contains("https://example.com/api-docs"), "got: {out}");
        assert!(out.contains("https://example.com/changelog"), "got: {out}");
        assert!(out.contains("接口库"), "got: {out}");
        // Layered tool guidance: own web-fetch tools first, the gateway tool
        // only as a conditional option, and an honest no-tools fallback.
        assert!(out.contains("web-fetch tool already available"), "got: {out}");
        assert!(out.contains("if a `nomi_knowledge_fetch_url` tool is available"), "got: {out}");
        assert!(out.contains("cannot read realtime sources"), "got: {out}");
        // The old unconditional promise must be gone — the gateway tool only
        // exists when the process-issued Platform Gateway is present.
        assert!(!out.contains("call the `nomi_knowledge_fetch_url` tool instead"), "got: {out}");
    }

    // ── writeback mode parsing ───────────────────────────────────────

    #[test]
    fn writeback_mode_parses_known_values_and_falls_back_to_staged() {
        assert_eq!(WritebackMode::parse(None), WritebackMode::Staged);
        assert_eq!(WritebackMode::parse(Some("staged")), WritebackMode::Staged);
        assert_eq!(WritebackMode::parse(Some("direct")), WritebackMode::Direct);
        // Unknown (or wrong-case) values must never pick the permissive mode.
        assert_eq!(WritebackMode::parse(Some("DIRECT")), WritebackMode::Staged);
        assert_eq!(WritebackMode::parse(Some("yolo")), WritebackMode::Staged);
        assert_eq!(WritebackMode::parse(Some("")), WritebackMode::Staged);
        assert_eq!(WritebackMode::default(), WritebackMode::Staged);
    }

    /// An unrecognized writeback_mode string renders the STAGED contract —
    /// never DIRECT (the permissive branch must be opt-in by exact value).
    #[test]
    fn unknown_writeback_mode_renders_staged_contract() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let out = build_knowledge_context(
            &[m],
            &prompt_opts(
                true,
                Some("yolo"),
                "0190f5fe-7c00-7a00-8000-000000000007",
            ),
        )
        .unwrap();
        assert!(out.contains("STAGED mode"), "got: {out}");
        assert!(
            out.contains("_inbox/0190f5fe-7c00-7a00-8000-000000000007/"),
            "got: {out}"
        );
        assert!(!out.contains("DIRECT mode"), "got: {out}");
    }

    // ── writeback eagerness (回写意识) ─────────────────────────────────

    #[test]
    fn writeback_eagerness_parses_known_values_and_falls_back_to_conservative() {
        assert_eq!(WritebackEagerness::parse(None), WritebackEagerness::Conservative);
        assert_eq!(WritebackEagerness::parse(Some("conservative")), WritebackEagerness::Conservative);
        assert_eq!(WritebackEagerness::parse(Some("aggressive")), WritebackEagerness::Aggressive);
        // Unknown / wrong-case values must never pick the eager mode.
        assert_eq!(WritebackEagerness::parse(Some("AGGRESSIVE")), WritebackEagerness::Conservative);
        assert_eq!(WritebackEagerness::parse(Some("bold")), WritebackEagerness::Conservative);
        assert_eq!(WritebackEagerness::parse(Some("")), WritebackEagerness::Conservative);
        assert_eq!(WritebackEagerness::default(), WritebackEagerness::Conservative);
    }

    /// Enabled write-back defaults to the CONSERVATIVE disposition clause and
    /// is orthogonal to the staged/direct placement decision.
    #[test]
    fn enabled_writeback_appends_conservative_clause_by_default() {
        let m = mount("库A", ".nomi/knowledge/库A");
        // Default eagerness (None) under staged mode.
        let staged = build_knowledge_context(
            &[m.clone()],
            &prompt_opts(true, None, "0190f5fe-7c00-7a00-8000-000000000001"),
        )
        .unwrap();
        assert!(staged.contains("STAGED mode"), "got: {staged}");
        assert!(staged.contains("Disposition — CONSERVATIVE"), "got: {staged}");
        assert!(!staged.contains("Disposition — AGGRESSIVE"), "got: {staged}");
        // Default eagerness under direct mode too.
        let direct = build_knowledge_context(
            &[m],
            &prompt_opts(
                true,
                Some("direct"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(direct.contains("DIRECT mode"), "got: {direct}");
        assert!(direct.contains("Disposition — CONSERVATIVE"), "got: {direct}");
    }

    /// The aggressive disposition renders its own clause, independent of mode.
    #[test]
    fn aggressive_eagerness_renders_aggressive_clause_for_both_modes() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let staged = build_knowledge_context(
            &[m.clone()],
            &prompt_opts_eager(
                true,
                Some("staged"),
                Some("aggressive"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(staged.contains("STAGED mode"), "got: {staged}");
        assert!(staged.contains("Disposition — AGGRESSIVE"), "got: {staged}");
        assert!(!staged.contains("Disposition — CONSERVATIVE"), "got: {staged}");
        // Staged placement survives an aggressive disposition (inbox still scoped).
        assert!(
            staged.contains("_inbox/0190f5fe-7c00-7a00-8000-000000000001/"),
            "got: {staged}"
        );

        let direct = build_knowledge_context(
            &[m],
            &prompt_opts_eager(
                true,
                Some("direct"),
                Some("aggressive"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(direct.contains("DIRECT mode"), "got: {direct}");
        assert!(direct.contains("Disposition — AGGRESSIVE"), "got: {direct}");
        assert!(!direct.contains("_inbox"), "got: {direct}");
    }

    /// Disabled write-back is read-only and carries no disposition clause —
    /// eagerness is meaningless without write-back.
    #[test]
    fn disabled_writeback_has_no_eagerness_clause() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let out = build_knowledge_context(
            &[m],
            &prompt_opts_eager(
                false,
                None,
                Some("aggressive"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(out.contains("Write-back is DISABLED"), "got: {out}");
        assert!(!out.contains("Disposition —"), "got: {out}");
    }

    // ── tool-based write-back contract (has_write_tool = true) ────────

    /// Options helper pinning has_write_tool = true (the nomi-engine surface).
    fn prompt_opts_tooled<'a>(mode: Option<&'a str>, target: &'a str) -> KnowledgeContextOptions<'a> {
        KnowledgeContextOptions {
            format: KnowledgeContextFormat::PromptSection,
            writeback: true,
            writeback_mode: mode,
            writeback_eagerness: None,
            target_id: target,
            has_search_tool: true,
            has_write_tool: true,
        }
    }

    /// When the surface has the native tool, the contract instructs CALLING
    /// `knowledge_write` and drops the file-path / inbox-path prose — in BOTH
    /// modes. The model must never be pointed at the generic Write tool.
    #[test]
    fn tool_contract_directs_to_knowledge_write_in_both_modes() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let staged = build_knowledge_context(
            &[m.clone()],
            &prompt_opts_tooled(
                Some("staged"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(staged.contains("knowledge_write"), "got: {staged}");
        assert!(staged.contains("STAGED mode"), "got: {staged}");
        assert!(staged.contains("Do NOT use the generic Write/Edit"), "got: {staged}");
        // The staged inbox PATH is now internal to the tool — never leaked to the model.
        assert!(!staged.contains("_inbox/"), "tool contract must not advertise the inbox path: {staged}");

        let direct = build_knowledge_context(
            &[m],
            &prompt_opts_tooled(
                Some("direct"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(direct.contains("knowledge_write"), "got: {direct}");
        assert!(direct.contains("DIRECT mode"), "got: {direct}");
        assert!(direct.contains("Do NOT use the generic Write/Edit"), "got: {direct}");
        // Disposition clause still appends under the tool contract.
        assert!(direct.contains("Disposition — CONSERVATIVE"), "got: {direct}");
    }

    /// Disabled write-back is read-only regardless of has_write_tool.
    #[test]
    fn tool_surface_still_read_only_when_writeback_disabled() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let opts = KnowledgeContextOptions {
            format: KnowledgeContextFormat::PromptSection,
            writeback: false,
            writeback_mode: None,
            writeback_eagerness: None,
            target_id: "0190f5fe-7c00-7a00-8000-000000000001",
            has_search_tool: true,
            has_write_tool: true,
        };
        let out = build_knowledge_context(&[m], &opts).unwrap();
        assert!(out.contains("Write-back is DISABLED"), "got: {out}");
        assert!(!out.contains("knowledge_write"), "got: {out}");
    }

    /// An unrecognized eagerness string renders the CONSERVATIVE clause — the
    /// eager branch must be opt-in by exact value, never reached by a typo.
    #[test]
    fn unknown_eagerness_renders_conservative_clause() {
        let m = mount("库A", ".nomi/knowledge/库A");
        let out = build_knowledge_context(
            &[m],
            &prompt_opts_eager(
                true,
                None,
                Some("yolo"),
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
        )
        .unwrap();
        assert!(out.contains("Disposition — CONSERVATIVE"), "got: {out}");
        assert!(!out.contains("Disposition — AGGRESSIVE"), "got: {out}");
    }

    // ── TerminalReadme format ────────────────────────────────────────

    #[test]
    fn terminal_readme_is_a_complete_document() {
        let mut m = mount("领域知识", ".nomi/knowledge/领域知识");
        m.toc = vec!["intro.md — 简介".into()];
        let opts = KnowledgeContextOptions {
            format: KnowledgeContextFormat::TerminalReadme,
            writeback: true,
            writeback_mode: None,
            writeback_eagerness: None,
            target_id: "0190f5fe-7c00-7a00-8000-000000000009",
            has_search_tool: false,
            has_write_tool: false,
        };
        let out = build_knowledge_context(&[m], &opts).unwrap();

        assert!(out.starts_with("# Knowledge bases"), "got: {out}");
        assert!(out.contains("NomiFun"), "got: {out}");
        // Relative-path baseline disambiguation (paths are workspace-rooted).
        assert!(out.contains("Paths below are relative to the workspace root."), "got: {out}");
        assert!(out.contains("## Retrieval protocol"), "got: {out}");
        assert!(out.contains("## Mounted bases"), "got: {out}");
        assert!(out.contains("### 领域知识"), "got: {out}");
        assert!(out.contains("intro.md — 简介"), "got: {out}");
        assert!(out.contains("STAGED mode"), "got: {out}");
        assert!(
            out.contains("_inbox/0190f5fe-7c00-7a00-8000-000000000009/"),
            "got: {out}"
        );
        // The prompt-section heading must not leak into the readme format.
        assert!(!out.contains("## Knowledge bases (extended knowledge source)"), "got: {out}");
    }

    // ── serde defaults for optional extra fields ─────────────────────

    #[test]
    fn mount_info_deserializes_extra_without_new_fields() {
        let minimal = serde_json::json!({
            "knowledge_base_id": "0190f5fe-7c00-7a00-8000-000000000001",
            "name": "运维手册",
            "description": "",
            "rel_path": ".nomi/knowledge/运维手册",
            "toc": ["deploy.md — 部署"],
        });
        let m: KnowledgeMountInfo =
            serde_json::from_value(minimal).expect("optional fields may be omitted");
        assert_eq!(m.summary, None);
        assert!(m.live_sources.is_empty());

        // New fields round-trip, and empty optionals stay off the wire.
        let bare = serde_json::to_value(mount("库A", ".nomi/knowledge/库A")).unwrap();
        assert!(bare.get("summary").is_none(), "got: {bare}");
        assert!(bare.get("live_sources").is_none(), "got: {bare}");

        let mut rich = mount("库B", ".nomi/knowledge/库B");
        rich.summary = Some("s".into());
        rich.live_sources = vec![KnowledgeSourceEntry {
            url: "https://e.com".into(),
            title: None,
            rendered: false,
        }];
        let v = serde_json::to_value(&rich).unwrap();
        let back: KnowledgeMountInfo = serde_json::from_value(v).unwrap();
        assert_eq!(back.summary.as_deref(), Some("s"));
        assert_eq!(back.live_sources.len(), 1);
    }
}
