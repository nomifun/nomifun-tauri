pub mod null_sink;
pub mod protocol_sink;
pub mod terminal;

use crossterm::execute;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use nomi_types::tool::ToolImage;
use std::io::{self, Write};

/// Result of delivering binary tool output to a user-visible sink.
///
/// The backend sink persists media and returns a model-facing locator. Other
/// sinks may leave media unmanaged (for example a terminal-only session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolMediaDelivery {
    Unmanaged,
    Delivered { context: String },
    Failed { error: String },
}

/// The durable output a high-signal generation/export tool is expected to
/// return. Browser screenshots are intentionally *not* classified here: they
/// are model context, not a user-requested generated artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactExpectation {
    None,
    Image,
    Audio,
    Video,
    File,
    Any,
}

impl ArtifactExpectation {
    pub fn accepts_mime(self, mime_type: &str) -> bool {
        let mime = mime_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match self {
            Self::None => false,
            Self::Image => mime.starts_with("image/"),
            Self::Audio => mime.starts_with("audio/"),
            Self::Video => mime.starts_with("video/"),
            // A file exporter/downloader may legitimately produce media. More
            // specific Image/Audio/Video expectations are selected first when
            // the tool identity names one of those products.
            Self::File => !mime.is_empty(),
            Self::Any => !mime.is_empty(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None | Self::Any => "artifact",
            Self::Image => "image artifact",
            Self::Audio => "audio artifact",
            Self::Video => "video artifact",
            Self::File => "file artifact",
        }
    }
}

/// A receipt-level artifact requirement.
///
/// Category variants preserve the legacy [`ArtifactExpectation`] behavior,
/// while format variants make a high-signal tool identity such as
/// `render_png` or `export_pdf` enforce the exact media type it promised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactRequirement {
    Image,
    Audio,
    Video,
    File,
    Any,
    Png,
    Jpeg,
    Webp,
    Gif,
    Mp3,
    Wav,
    Flac,
    Ogg,
    M4a,
    Mp4,
    Webm,
    Mov,
    Pdf,
    Zip,
    Docx,
    Xlsx,
    Pptx,
    Csv,
    Json,
    Markdown,
    Html,
    Xml,
}

impl ArtifactRequirement {
    pub fn expectation(self) -> ArtifactExpectation {
        match self {
            Self::Image | Self::Png | Self::Jpeg | Self::Webp | Self::Gif => {
                ArtifactExpectation::Image
            }
            Self::Audio | Self::Mp3 | Self::Wav | Self::Flac | Self::Ogg | Self::M4a => {
                ArtifactExpectation::Audio
            }
            Self::Video | Self::Mp4 | Self::Webm | Self::Mov => ArtifactExpectation::Video,
            Self::File
            | Self::Pdf
            | Self::Zip
            | Self::Docx
            | Self::Xlsx
            | Self::Pptx
            | Self::Csv
            | Self::Json
            | Self::Markdown
            | Self::Html
            | Self::Xml => ArtifactExpectation::File,
            Self::Any => ArtifactExpectation::Any,
        }
    }

    pub fn exact_mime(self) -> Option<&'static str> {
        match self {
            Self::Png => Some("image/png"),
            Self::Jpeg => Some("image/jpeg"),
            Self::Webp => Some("image/webp"),
            Self::Gif => Some("image/gif"),
            Self::Mp3 => Some("audio/mpeg"),
            Self::Wav => Some("audio/wav"),
            Self::Flac => Some("audio/flac"),
            Self::Ogg => Some("audio/ogg"),
            Self::M4a => Some("audio/mp4"),
            Self::Mp4 => Some("video/mp4"),
            Self::Webm => Some("video/webm"),
            Self::Mov => Some("video/quicktime"),
            Self::Pdf => Some("application/pdf"),
            Self::Zip => Some("application/zip"),
            Self::Docx => Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            Self::Xlsx => {
                Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
            }
            Self::Pptx => Some(
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            Self::Csv => Some("text/csv"),
            Self::Json => Some("application/json"),
            Self::Markdown => Some("text/markdown"),
            Self::Html => Some("text/html"),
            Self::Xml => Some("application/xml"),
            Self::Image | Self::Audio | Self::Video | Self::File | Self::Any => None,
        }
    }

    /// Match a validated receipt MIME. Parameters and ASCII case are ignored,
    /// but exact-format requirements do not accept MIME aliases or a generic
    /// `application/octet-stream` receipt.
    pub fn accepts_mime(self, mime_type: &str) -> bool {
        let mime = mime_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        self.exact_mime()
            .map_or_else(|| self.expectation().accepts_mime(&mime), |exact| mime == exact)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "image artifact",
            Self::Audio => "audio artifact",
            Self::Video => "video artifact",
            Self::File => "file artifact",
            Self::Any => "artifact",
            Self::Png => "PNG image artifact",
            Self::Jpeg => "JPEG image artifact",
            Self::Webp => "WebP image artifact",
            Self::Gif => "GIF image artifact",
            Self::Mp3 => "MP3 audio artifact",
            Self::Wav => "WAV audio artifact",
            Self::Flac => "FLAC audio artifact",
            Self::Ogg => "Ogg audio artifact",
            Self::M4a => "M4A audio artifact",
            Self::Mp4 => "MP4 video artifact",
            Self::Webm => "WebM video artifact",
            Self::Mov => "QuickTime video artifact",
            Self::Pdf => "PDF artifact",
            Self::Zip => "ZIP archive artifact",
            Self::Docx => "DOCX document artifact",
            Self::Xlsx => "XLSX workbook artifact",
            Self::Pptx => "PPTX presentation artifact",
            Self::Csv => "CSV artifact",
            Self::Json => "JSON artifact",
            Self::Markdown => "Markdown artifact",
            Self::Html => "HTML artifact",
            Self::Xml => "XML artifact",
        }
    }

    fn from_expectation(expectation: ArtifactExpectation) -> Option<Self> {
        match expectation {
            ArtifactExpectation::None => None,
            ArtifactExpectation::Image => Some(Self::Image),
            ArtifactExpectation::Audio => Some(Self::Audio),
            ArtifactExpectation::Video => Some(Self::Video),
            ArtifactExpectation::File => Some(Self::File),
            ArtifactExpectation::Any => Some(Self::Any),
        }
    }
}

/// A complete artifact obligation inferred before a tool executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactContract {
    /// Retained explicitly for compatibility with consumers that route by the
    /// existing coarse artifact kind.
    pub expectation: ArtifactExpectation,
    /// Receipt-level category or exact-format requirement.
    pub requirement: ArtifactRequirement,
    /// An explicitly requested output quantity. `None` means the implicit
    /// single-output contract, which remains distinguishable during partial
    /// tool metadata merges from an explicit `count: 1`.
    pub requested_count: Option<usize>,
}

impl ArtifactContract {
    pub fn expected_count(self) -> usize {
        self.requested_count.unwrap_or(1)
    }

    pub fn accepts_mime(self, mime_type: &str) -> bool {
        self.requirement.accepts_mime(mime_type)
    }

    pub fn label(self) -> &'static str {
        self.requirement.label()
    }

    /// Validate both the minimum number and MIME of a completed call's receipts.
    /// A generator may safely return additional valid outputs; it may not return
    /// fewer than the quantity the caller explicitly requested.
    pub fn validate_mimes<T: AsRef<str>>(
        self,
        mime_types: &[T],
    ) -> Result<(), ArtifactContractViolation> {
        if mime_types.len() < self.expected_count() {
            return Err(ArtifactContractViolation::Count {
                expected: self.expected_count(),
                actual: mime_types.len(),
            });
        }
        for (index, mime_type) in mime_types.iter().enumerate() {
            if !self.accepts_mime(mime_type.as_ref()) {
                return Err(ArtifactContractViolation::Mime {
                    index,
                    requirement: self.requirement,
                });
            }
        }
        Ok(())
    }

    /// Merge independently observed metadata for the same call. Broad
    /// requirements can be narrowed, while incompatible formats/kinds and two
    /// different explicit quantities fail closed.
    pub fn merge(self, observed: Self) -> Result<Self, ArtifactContractMergeError> {
        let requirement = merge_artifact_requirements(self.requirement, observed.requirement)
            .ok_or(ArtifactContractMergeError::Requirement {
                existing: self.requirement,
                observed: observed.requirement,
            })?;
        let requested_count = match (self.requested_count, observed.requested_count) {
            (Some(existing), Some(observed)) if existing != observed => {
                return Err(ArtifactContractMergeError::Count { existing, observed });
            }
            (Some(count), _) | (_, Some(count)) => Some(count),
            (None, None) => None,
        };
        Ok(Self {
            expectation: requirement.expectation(),
            requirement,
            requested_count,
        })
    }
}

fn merge_artifact_requirements(
    existing: ArtifactRequirement,
    observed: ArtifactRequirement,
) -> Option<ArtifactRequirement> {
    use ArtifactRequirement::*;
    if existing == observed {
        return Some(existing);
    }
    if existing == Any || existing == File {
        return Some(observed);
    }
    if observed == Any || observed == File {
        return Some(existing);
    }
    match (existing, observed) {
        (Image, exact @ (Png | Jpeg | Webp | Gif))
        | (exact @ (Png | Jpeg | Webp | Gif), Image) => Some(exact),
        (Audio, exact @ (Mp3 | Wav | Flac | Ogg | M4a))
        | (exact @ (Mp3 | Wav | Flac | Ogg | M4a), Audio) => Some(exact),
        (Video, exact @ (Mp4 | Webm | Mov))
        | (exact @ (Mp4 | Webm | Mov), Video) => Some(exact),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactContractMergeError {
    Requirement {
        existing: ArtifactRequirement,
        observed: ArtifactRequirement,
    },
    Count {
        existing: usize,
        observed: usize,
    },
}

impl std::fmt::Display for ArtifactContractMergeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Requirement { existing, observed } => write!(
                formatter,
                "conflicting artifact requirements: {} vs {}",
                existing.label(),
                observed.label()
            ),
            Self::Count { existing, observed } => write!(
                formatter,
                "conflicting artifact output counts: {existing} vs {observed}"
            ),
        }
    }
}

impl std::error::Error for ArtifactContractMergeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactContractViolation {
    Count {
        expected: usize,
        actual: usize,
    },
    Mime {
        index: usize,
        requirement: ArtifactRequirement,
    },
}

impl std::fmt::Display for ArtifactContractViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count { expected, actual } => {
                write!(
                    formatter,
                    "expected at least {expected} artifact receipt(s), got {actual}"
                )
            }
            Self::Mime { index, requirement } => write!(
                formatter,
                "artifact receipt {index} does not satisfy {}",
                requirement.label()
            ),
        }
    }
}

impl std::error::Error for ArtifactContractViolation {}

pub const MAX_ARTIFACT_OUTPUT_COUNT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactCountError {
    InvalidValue { field: &'static str },
    ExceedsLimit {
        field: &'static str,
        value: u64,
        limit: usize,
    },
    ConflictingValues,
}

impl std::fmt::Display for ArtifactCountError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidValue { field } => {
                write!(formatter, "artifact output count `{field}` must be a positive integer")
            }
            Self::ExceedsLimit {
                field,
                value,
                limit,
            } => write!(
                formatter,
                "artifact output count `{field}` ({value}) exceeds the limit ({limit})"
            ),
            Self::ConflictingValues => {
                formatter.write_str("artifact output count fields contain conflicting values")
            }
        }
    }
}

impl std::error::Error for ArtifactCountError {}

/// Parse a top-level image-generation quantity. Unknown fields are ignored;
/// recognized fields are fail-closed when malformed, excessive, or mutually
/// inconsistent.
pub fn parse_artifact_output_count(
    input: &serde_json::Value,
) -> Result<Option<usize>, ArtifactCountError> {
    const FIELDS: &[&str] = &["count", "n", "num_images", "num_outputs"];
    parse_artifact_output_count_fields(input, FIELDS)
}

fn parse_general_artifact_output_count(
    input: &serde_json::Value,
) -> Result<Option<usize>, ArtifactCountError> {
    // `num_images` is intentionally image-only; generic exporters commonly
    // use the remaining names for an explicit number of deliverables.
    const FIELDS: &[&str] = &["num_outputs", "num_files", "num_artifacts"];
    parse_artifact_output_count_fields(input, FIELDS)
}

fn parse_artifact_output_count_fields(
    input: &serde_json::Value,
    fields: &'static [&'static str],
) -> Result<Option<usize>, ArtifactCountError> {
    let Some(object) = input.as_object() else {
        return Ok(None);
    };
    let mut parsed = None;
    for field in fields {
        let Some(value) = object.get(*field) else {
            continue;
        };
        let Some(value) = value.as_u64().filter(|value| *value > 0) else {
            return Err(ArtifactCountError::InvalidValue { field });
        };
        if value > MAX_ARTIFACT_OUTPUT_COUNT as u64 {
            return Err(ArtifactCountError::ExceedsLimit {
                field,
                value,
                limit: MAX_ARTIFACT_OUTPUT_COUNT,
            });
        }
        let value = value as usize;
        if parsed.is_some_and(|previous| previous != value) {
            return Err(ArtifactCountError::ConflictingValues);
        }
        parsed = Some(value);
    }
    Ok(parsed)
}

fn tool_identity_words(name: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(name.len());
    let mut previous_was_lower_or_digit = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_was_lower_or_digit {
                normalized.push(' ');
            }
            normalized.push(character.to_ascii_lowercase());
            previous_was_lower_or_digit = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            normalized.push(' ');
            previous_was_lower_or_digit = false;
        }
    }
    normalized
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn identity_word_matches(word: &str, expected: &str) -> bool {
    word == expected || word.strip_suffix('s') == Some(expected)
}

fn identity_has(words: &[String], expected: &[&str]) -> bool {
    words
        .iter()
        .any(|word| expected.iter().any(|candidate| identity_word_matches(word, candidate)))
}

fn identity_has_compound(words: &[String], actions: &[&str], products: &[&str]) -> bool {
    words.iter().any(|word| {
        actions.iter().any(|action| {
            word.strip_prefix(action).is_some_and(|product| {
                !product.is_empty()
                    && products
                        .iter()
                        .any(|candidate| identity_word_matches(product, candidate))
            })
        })
    })
}

const TRANSFORM_ACTIONS: &[&str] = &["convert", "transform", "transcode"];
const PACKAGE_ACTIONS: &[&str] = &["package", "bundle"];
const ACTION_ARTIFACT_PRODUCTS: &[&str] = &[
    "artifact",
    "file",
    "archive",
    "image",
    "picture",
    "photo",
    "illustration",
    "poster",
    "logo",
    "icon",
    "audio",
    "speech",
    "music",
    "podcast",
    "video",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "wav",
    "mp3",
    "flac",
    "ogg",
    "m4a",
    "mp4",
    "mov",
    "m4v",
    "webm",
    "zip",
    "pdf",
    "docx",
    "xlsx",
    "pptx",
    "csv",
    "json",
    "markdown",
    "md",
    "html",
    "htm",
    "xml",
    "document",
    "report",
    "presentation",
    "slide",
    "deck",
    "spreadsheet",
    "workbook",
];

fn identity_names_artifact_action(words: &[String], actions: &[&str]) -> bool {
    (identity_has(words, actions) && identity_has(words, ACTION_ARTIFACT_PRODUCTS))
        || identity_has_compound(words, actions, ACTION_ARTIFACT_PRODUCTS)
}

fn identity_names_product_modifier(words: &[String]) -> bool {
    const PRODUCTS: &[&str] = &[
        "image",
        "picture",
        "photo",
        "audio",
        "speech",
        "podcast",
        "video",
        "file",
        "pdf",
        "docx",
        "xlsx",
        "pptx",
        "csv",
        "json",
        "markdown",
        "html",
        "xml",
        "presentation",
        "spreadsheet",
        "document",
    ];
    const MODIFIERS: &[&str] = &[
        "analyzer",
        "analyser",
        "analysis",
        "classifier",
        "decoder",
        "encoder",
        "parser",
        "processor",
        "model",
        "pipeline",
        "workflow",
        "tool",
        "service",
        "system",
        "prompt",
        "editor",
        "viewer",
        "metadata",
        "preview",
        "annotation",
        "annotations",
    ];
    words.windows(2).any(|pair| {
        identity_has(&pair[..1], PRODUCTS) && identity_has(&pair[1..], MODIFIERS)
    })
}

fn artifact_product_words(words: &[String]) -> &[String] {
    if !identity_names_artifact_action(words, TRANSFORM_ACTIONS) {
        return words;
    }
    words
        .iter()
        .rposition(|word| matches!(word.as_str(), "to" | "into" | "as"))
        .and_then(|index| words.get(index + 1..))
        .filter(|suffix| !suffix.is_empty())
        .unwrap_or(words)
}

/// Infer an artifact contract only from tool identity, never from prompt/body
/// text. This keeps ordinary screenshot/read tools unmanaged while preventing
/// names such as `image_gen`, `render_video`, or `export_report` from claiming
/// success with text alone.
pub fn artifact_expectation(name: &str) -> ArtifactExpectation {
    let words = tool_identity_words(name);
    let joined = words.join("_");

    // Packaging names describe their inputs (`package_images`) far more often
    // than their final container. Require a durable file, but do not mistake
    // the packaged input kind for the output kind. Exact ZIP remains narrowed
    // later when the identity explicitly names it.
    if identity_names_artifact_action(&words, PACKAGE_ACTIONS) {
        return ArtifactExpectation::File;
    }
    // `create_image_decoder` and `design_pdf_parser` name software/context
    // built around an artifact type; the artifact word is not their output.
    if identity_names_product_modifier(&words) {
        return ArtifactExpectation::None;
    }

    let names_image_generator = matches!(
        joined.as_str(),
        "image_gen" | "imagegeneration" | "image_generation" | "generate_image" | "create_image"
    ) || words
        .iter()
        .any(|word| matches!(word.as_str(), "imagegen" | "imagegeneration"))
        || words.windows(2).any(|pair| {
            identity_word_matches(&pair[0], "image")
                && matches!(pair[1].as_str(), "gen" | "generation" | "generator")
                || matches!(pair[0].as_str(), "generate" | "create")
                    && identity_word_matches(&pair[1], "image")
        })
        || identity_has_compound(&words, &["generate", "create"], &["image"]);
    if names_image_generator
        || (identity_has(&words, &["nomifun"])
            && identity_has(&words, &["image"])
            && identity_has(&words, &["generation"]))
    {
        return ArtifactExpectation::Image;
    }

    if matches!(joined.as_str(), "text_to_speech" | "tts" | "speech_synthesis")
        || identity_has(&words, &["tts"])
        || words.windows(2).any(|pair| pair[0] == "speech" && pair[1] == "synthesis")
        || words
            .windows(3)
            .any(|triple| triple[0] == "text" && triple[1] == "to" && triple[2] == "speech")
        || words
            .iter()
            .any(|word| matches!(word.as_str(), "texttospeech" | "speechsynthesis"))
    {
        return ArtifactExpectation::Audio;
    }

    const ACTIONS: &[&str] = &[
        "generate",
        "create",
        "render",
        "draw",
        "design",
        "convert",
        "transform",
        "transcode",
        "record",
        "package",
        "bundle",
        "export",
        "produce",
        "synthesize",
        "synthesis",
        "generation",
        "generator",
        "save",
        "download",
    ];
    const IMAGE_PRODUCTS: &[&str] = &[
        "image",
        "picture",
        "photo",
        "illustration",
        "poster",
        "logo",
        "icon",
        "infographic",
        "diagram",
        "flowchart",
        "chart",
        "thumbnail",
        "mockup",
        "png",
        "jpg",
        "jpeg",
        "gif",
        "webp",
    ];
    const AUDIO_PRODUCTS: &[&str] = &[
        "audio",
        "speech",
        "music",
        "podcast",
        "narration",
        "voiceover",
        "soundtrack",
        "song",
        "recording",
        "tts",
        "wav",
        "mp3",
        "flac",
        "ogg",
        "m4a",
    ];
    const VIDEO_PRODUCTS: &[&str] = &["video", "mp4", "mov", "m4v", "webm"];
    const FILE_PRODUCTS: &[&str] = &[
        "file",
        "archive",
        "zip",
        "pdf",
        "docx",
        "xlsx",
        "pptx",
        "csv",
        "json",
        "markdown",
        "md",
        "html",
        "htm",
        "xml",
        "txt",
        "tsv",
        "tar",
        "tgz",
        "rar",
        "7z",
        "document",
        "report",
        "presentation",
        "slide",
        "deck",
        "spreadsheet",
        "workbook",
    ];
    let product_words = artifact_product_words(&words);
    let target_is_explicit = product_words.len() != words.len();
    // `convert_pdf` does not say whether PDF is the source or destination.
    // Conversion still promises an artifact, but only a directional identity
    // such as `convert_png_to_pdf` may infer an exact target kind/format.
    if identity_names_artifact_action(&words, TRANSFORM_ACTIONS) && !target_is_explicit {
        return ArtifactExpectation::File;
    }
    let has_action = identity_has(&words, ACTIONS);
    let compound_image = !target_is_explicit && identity_has_compound(&words, ACTIONS, IMAGE_PRODUCTS);
    let compound_audio = !target_is_explicit && identity_has_compound(&words, ACTIONS, AUDIO_PRODUCTS);
    let compound_video = !target_is_explicit && identity_has_compound(&words, ACTIONS, VIDEO_PRODUCTS);
    let compound_file = !target_is_explicit && identity_has_compound(&words, ACTIONS, FILE_PRODUCTS);
    let compound_artifact = identity_has_compound(&words, ACTIONS, &["artifact"]);
    if !has_action
        && !compound_image
        && !compound_audio
        && !compound_video
        && !compound_file
        && !compound_artifact
    {
        return ArtifactExpectation::None;
    }
    if compound_image || identity_has(product_words, IMAGE_PRODUCTS) {
        ArtifactExpectation::Image
    } else if compound_audio || identity_has(product_words, AUDIO_PRODUCTS) {
        ArtifactExpectation::Audio
    } else if compound_video || identity_has(product_words, VIDEO_PRODUCTS) {
        ArtifactExpectation::Video
    } else if compound_file || identity_has(product_words, FILE_PRODUCTS) {
        ArtifactExpectation::File
    } else if compound_artifact || identity_has(&words, &["artifact"]) {
        ArtifactExpectation::Any
    } else {
        ArtifactExpectation::None
    }
}

fn exact_artifact_requirement(
    words: &[String],
    expectation: ArtifactExpectation,
) -> Option<ArtifactRequirement> {
    const FORMAT_ACTIONS: &[&str] = &[
        "generate",
        "create",
        "render",
        "draw",
        "design",
        "convert",
        "transform",
        "transcode",
        "record",
        "package",
        "bundle",
        "export",
        "produce",
        "synthesize",
        "save",
        "download",
    ];
    let product_words = artifact_product_words(words);
    let target_is_explicit = product_words.len() != words.len();
    if identity_names_artifact_action(words, PACKAGE_ACTIONS) {
        return (identity_has(words, &["zip"])
            || identity_has_compound(words, PACKAGE_ACTIONS, &["zip"]))
        .then_some(ArtifactRequirement::Zip);
    }
    if identity_names_artifact_action(words, TRANSFORM_ACTIONS) && !target_is_explicit {
        return None;
    }
    let format_is_named = |names: &[&str]| {
        identity_has(product_words, names)
            || (!target_is_explicit && identity_has_compound(words, FORMAT_ACTIONS, names))
    };
    let candidates: &[(ArtifactRequirement, bool)] = match expectation {
        ArtifactExpectation::Image => &[
            (ArtifactRequirement::Png, format_is_named(&["png"])),
            (
                ArtifactRequirement::Jpeg,
                format_is_named(&["jpg", "jpeg"]),
            ),
            (ArtifactRequirement::Webp, format_is_named(&["webp"])),
            (ArtifactRequirement::Gif, format_is_named(&["gif"])),
        ],
        ArtifactExpectation::Audio => &[
            (ArtifactRequirement::Mp3, format_is_named(&["mp3"])),
            (ArtifactRequirement::Wav, format_is_named(&["wav"])),
            (ArtifactRequirement::Flac, format_is_named(&["flac"])),
            (ArtifactRequirement::Ogg, format_is_named(&["ogg"])),
            (ArtifactRequirement::M4a, format_is_named(&["m4a"])),
        ],
        ArtifactExpectation::Video => &[
            (
                ArtifactRequirement::Mp4,
                format_is_named(&["mp4", "m4v"]),
            ),
            (ArtifactRequirement::Webm, format_is_named(&["webm"])),
            (ArtifactRequirement::Mov, format_is_named(&["mov"])),
        ],
        ArtifactExpectation::File => &[
            (ArtifactRequirement::Pdf, format_is_named(&["pdf"])),
            (ArtifactRequirement::Zip, format_is_named(&["zip"])),
            (ArtifactRequirement::Docx, format_is_named(&["docx"])),
            (ArtifactRequirement::Xlsx, format_is_named(&["xlsx"])),
            (ArtifactRequirement::Pptx, format_is_named(&["pptx"])),
            (ArtifactRequirement::Csv, format_is_named(&["csv"])),
            (ArtifactRequirement::Json, format_is_named(&["json"])),
            (
                ArtifactRequirement::Markdown,
                format_is_named(&["markdown", "md"]),
            ),
            (ArtifactRequirement::Html, format_is_named(&["html", "htm"])),
            (ArtifactRequirement::Xml, format_is_named(&["xml"])),
        ],
        ArtifactExpectation::None | ArtifactExpectation::Any => &[],
    };
    let mut matches = candidates
        .iter()
        .filter_map(|(requirement, matches)| matches.then_some(*requirement));
    let requirement = matches.next()?;
    // Ambiguous identities (for example `export_png_jpeg`) retain their safe
    // category contract instead of guessing which format is the output.
    matches.next().is_none().then_some(requirement)
}

/// Infer the richer receipt contract for a high-signal tool identity.
/// Ordinary read/edit/inspect identities return `None`.
pub fn artifact_contract(name: &str) -> Option<ArtifactContract> {
    let expectation = artifact_expectation(name);
    let fallback = ArtifactRequirement::from_expectation(expectation)?;
    let words = tool_identity_words(name);
    let requirement = exact_artifact_requirement(&words, expectation).unwrap_or(fallback);
    Some(ArtifactContract {
        expectation,
        requirement,
        requested_count: None,
    })
}

/// Infer a tool identity contract and apply an explicit image quantity from
/// the raw tool input. Quantity-like fields on non-image tools are ignored so
/// ordinary exporter parameters cannot accidentally broaden their obligation.
pub fn artifact_contract_with_input(
    name: &str,
    input: &serde_json::Value,
) -> Result<Option<ArtifactContract>, ArtifactCountError> {
    let Some(mut contract) = artifact_contract(name) else {
        return Ok(None);
    };
    contract.requested_count = if contract.expectation == ArtifactExpectation::Image {
        parse_artifact_output_count(input)?
    } else {
        parse_general_artifact_output_count(input)?
    };
    Ok(Some(contract))
}

pub fn is_context_only_image_tool(name: &str) -> bool {
    let words = tool_identity_words(name);
    words.iter().any(|word| word == "screenshot")
        || words
            .iter()
            .any(|word| matches!(word.as_str(), "viewimage" | "imageviewer"))
        || matches!(words.as_slice(), [tool] if matches!(tool.as_str(), "browser" | "computer"))
        || words.windows(2).any(|pair| {
            matches!(
                pair,
                [first, second]
                    if matches!(first.as_str(), "view" | "read" | "inspect")
                        && identity_word_matches(second, "image")
            )
        })
}

/// Abstraction over output channels (terminal vs JSON stream protocol)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRetryContext {
    pub retry_group_id: String,
    pub attempt_no: u32,
    pub retry_of_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallExecutionContext {
    pub input: serde_json::Value,
    pub retry: ToolCallRetryContext,
}

pub trait OutputSink: Send + Sync {
    /// Stream text delta from LLM
    fn emit_text_delta(&self, text: &str, msg_id: &str);
    /// Stream thinking content from LLM
    fn emit_thinking(&self, text: &str, msg_id: &str);
    /// Publish a committed, validated tool call as Running.
    fn emit_tool_call(&self, tool_use_id: &str, name: &str, input: &str);
    /// Publish a tool call while preserving the untruncated identity used for
    /// artifact classification. Existing sinks remain source-compatible.
    fn emit_tool_call_with_artifact_identity(
        &self,
        tool_use_id: &str,
        name: &str,
        _artifact_identity: &str,
        input: &str,
    ) {
        self.emit_tool_call(tool_use_id, name, input);
    }
    /// Publish a committed tool call with durable execution identity. Existing
    /// sinks remain compatible; structured sinks can override this to persist
    /// retry grouping without changing terminal/CLI output.
    fn emit_tool_call_with_context(
        &self,
        tool_use_id: &str,
        name: &str,
        artifact_identity: &str,
        input: &str,
        _context: &ToolCallExecutionContext,
    ) {
        self.emit_tool_call_with_artifact_identity(
            tool_use_id,
            name,
            artifact_identity,
            input,
        );
    }
    /// Surface non-terminal model activity when the provider stream is still
    /// alive but has not produced a new visible event for a short period.
    fn emit_model_activity(&self, _msg_id: &str, _status: &str) {}
    /// Display tool result.
    fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, content: &str);
    /// Deliver inline artifact blocks attached to a tool result. The legacy
    /// method name and carrier type remain wire-compatible. Context-only
    /// screenshots may remain unmanaged, but a high-signal generation/export
    /// tool and every non-image payload fail closed unless a sink explicitly
    /// persists them and returns [`ToolMediaDelivery::Delivered`].
    fn emit_tool_result_with_images(
        &self,
        tool_use_id: &str,
        name: &str,
        is_error: bool,
        content: &str,
        images: &[ToolImage],
    ) -> ToolMediaDelivery {
        if is_error {
            self.emit_tool_result(tool_use_id, name, true, content);
            return ToolMediaDelivery::Unmanaged;
        }
        let expectation = artifact_expectation(name);
        let has_non_image = images
            .iter()
            .any(|artifact| !artifact.media_type.trim().to_ascii_lowercase().starts_with("image/"));
        let missing_expected = expectation != ArtifactExpectation::None
            && !images
                .iter()
                .any(|artifact| expectation.accepts_mime(&artifact.media_type));
        let cannot_deliver_expected =
            expectation != ArtifactExpectation::None && !images.is_empty();
        let cannot_deliver_unclassified_images = expectation == ArtifactExpectation::None
            && !images.is_empty()
            && !is_context_only_image_tool(name);

        if has_non_image
            || missing_expected
            || cannot_deliver_expected
            || cannot_deliver_unclassified_images
        {
            let error = if missing_expected {
                format!("tool returned no {}", expectation.label())
            } else {
                "this output sink cannot persist generated tool artifacts".to_owned()
            };
            let output = if content.trim().is_empty() {
                format!("Artifact delivery failed: {error}")
            } else {
                format!("{content}\nArtifact delivery failed: {error}")
            };
            self.emit_tool_result(tool_use_id, name, true, &output);
            return ToolMediaDelivery::Failed { error };
        }
        self.emit_tool_result(tool_use_id, name, is_error, content);
        ToolMediaDelivery::Unmanaged
    }
    /// Deliver a result while preserving the untruncated identity used for
    /// artifact classification. The default delegates to the legacy method.
    fn emit_tool_result_with_images_and_artifact_identity(
        &self,
        tool_use_id: &str,
        name: &str,
        _artifact_identity: &str,
        is_error: bool,
        content: &str,
        images: &[ToolImage],
    ) -> ToolMediaDelivery {
        self.emit_tool_result_with_images(tool_use_id, name, is_error, content, images)
    }
    /// Deliver a result with the same immutable execution identity used for
    /// the Running event. This is also used for pre-dispatch validation errors,
    /// where no Running event was emitted.
    fn emit_tool_result_with_images_and_context(
        &self,
        tool_use_id: &str,
        name: &str,
        artifact_identity: &str,
        is_error: bool,
        content: &str,
        images: &[ToolImage],
        _context: &ToolCallExecutionContext,
    ) -> ToolMediaDelivery {
        self.emit_tool_result_with_images_and_artifact_identity(
            tool_use_id,
            name,
            artifact_identity,
            is_error,
            content,
            images,
        )
    }
    /// Signal start of a new message stream
    fn emit_stream_start(&self, msg_id: &str);
    /// Signal end of a message stream with usage stats
    fn emit_stream_end(
        &self,
        msg_id: &str,
        turns: usize,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
    );
    /// Display error
    fn emit_error(&self, msg: &str);
    /// Display informational message
    fn emit_info(&self, msg: &str);
    /// Display a non-fatal warning: a benign, recoverable diagnostic where the
    /// turn/session still completes successfully (autocompact failure, session
    /// save/index hiccup, MCP-init failure, `/compact` failure). Unlike
    /// `emit_error`, a warning must NOT be treated as a turn-failing condition by
    /// downstream consumers — the AutoWork runner classifies
    /// an `Error` stream event as a FAILED turn (re-pend / burn attempt / pause
    /// tag). The default routes to `emit_info` (non-fatal); sinks that carry a
    /// severity level on the wire (the backend stream bridge) override it.
    fn emit_warning(&self, msg: &str) {
        self.emit_info(msg);
    }
}

pub struct OutputFormatter {
    color_enabled: bool,
}

impl OutputFormatter {
    pub fn new(no_color: bool) -> Self {
        // Also check NO_COLOR env var (standard: https://no-color.org/)
        let color_enabled = !no_color
            && std::env::var("NO_COLOR").is_err()
            && is_terminal::is_terminal(io::stderr());
        Self { color_enabled }
    }

    /// Print LLM text delta (streaming, no newline)
    pub fn text_delta(&self, text: &str) {
        print!("{}", text);
        let _ = io::stdout().flush();
    }

    /// Print tool call announcement
    pub fn tool_call(&self, name: &str, input: &str) {
        if self.color_enabled {
            let mut stderr = io::stderr();
            let _ = execute!(
                stderr,
                SetForegroundColor(Color::Cyan),
                SetAttribute(Attribute::Bold),
                Print(format!("\n[tool] {}", name)),
                ResetColor,
                SetForegroundColor(Color::DarkGrey),
                Print(format!("({})\n", truncate_display(input, 200))),
                ResetColor,
            );
        } else {
            eprintln!("\n[tool] {}({})", name, truncate_display(input, 200));
        }
    }

    /// Print tool result
    pub fn tool_result(&self, name: &str, is_error: bool, content: &str) {
        if self.color_enabled {
            let color = if is_error { Color::Red } else { Color::Green };
            let attr = if is_error {
                Attribute::Bold
            } else {
                Attribute::Dim
            };
            let mut stderr = io::stderr();
            let _ = execute!(
                stderr,
                SetForegroundColor(color),
                SetAttribute(attr),
                Print(format!("[{}] {}\n", name, truncate_display(content, 500))),
                ResetColor,
            );
        } else {
            let prefix = if is_error { "ERROR" } else { "OK" };
            eprintln!("[{} {}] {}", name, prefix, truncate_display(content, 500));
        }
    }

    /// Print thinking content
    pub fn thinking(&self, text: &str) {
        if self.color_enabled {
            let mut stderr = io::stderr();
            let _ = execute!(
                stderr,
                SetForegroundColor(Color::DarkGrey),
                SetAttribute(Attribute::Italic),
                Print(text),
                ResetColor,
            );
        }
        // Silent in no-color mode (thinking is optional display)
    }

    /// Print turn summary stats
    pub fn turn_stats(
        &self,
        turns: usize,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
    ) {
        let cache_info = if cache_creation_tokens > 0 || cache_read_tokens > 0 {
            format!(
                " | cache: {} created, {} read",
                cache_creation_tokens, cache_read_tokens
            )
        } else {
            String::new()
        };

        let cached_suffix = if cache_read_tokens > 0 {
            format!(" ({} cached)", cache_read_tokens)
        } else {
            String::new()
        };

        if self.color_enabled {
            let mut stderr = io::stderr();
            let _ = execute!(
                stderr,
                SetForegroundColor(Color::Yellow),
                SetAttribute(Attribute::Dim),
                Print(format!(
                    "\n[turns: {} | tokens: {} in{} / {} out{}]\n",
                    turns, input_tokens, cached_suffix, output_tokens, cache_info
                )),
                ResetColor,
            );
        } else {
            eprintln!(
                "\n[turns: {} | tokens: {} in{} / {} out{}]",
                turns, input_tokens, cached_suffix, output_tokens, cache_info
            );
        }
    }

    /// Print REPL prompt
    pub fn repl_prompt(&self) {
        if self.color_enabled {
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::Green),
                SetAttribute(Attribute::Bold),
                Print("\n> "),
                ResetColor,
            );
            let _ = stdout.flush();
        } else {
            print!("\n> ");
            let _ = io::stdout().flush();
        }
    }

    /// Print error
    pub fn error(&self, msg: &str) {
        if self.color_enabled {
            let mut stderr = io::stderr();
            let _ = execute!(
                stderr,
                SetForegroundColor(Color::Red),
                Print(format!("[error] {}\n", msg)),
                ResetColor,
            );
        } else {
            eprintln!("[error] {}", msg);
        }
    }

    /// Print session info
    pub fn session_info(&self, msg: &str) {
        if self.color_enabled {
            let mut stderr = io::stderr();
            let _ = execute!(
                stderr,
                SetForegroundColor(Color::Blue),
                SetAttribute(Attribute::Dim),
                Print(format!("{}\n", msg)),
                ResetColor,
            );
        } else {
            eprintln!("{}", msg);
        }
    }
}

fn truncate_display(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a char boundary to avoid panicking on multi-byte characters
        let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatter_no_color_mode() {
        // Verify construction with no_color=true does not panic
        let _formatter = OutputFormatter::new(true);
    }

    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<(bool, String)>>);

    impl OutputSink for RecordingSink {
        fn emit_text_delta(&self, _text: &str, _msg_id: &str) {}
        fn emit_thinking(&self, _text: &str, _msg_id: &str) {}
        fn emit_tool_call(&self, _tool_use_id: &str, _name: &str, _input: &str) {}
        fn emit_tool_result(&self, _tool_use_id: &str, _name: &str, is_error: bool, content: &str) {
            self.0.lock().unwrap().push((is_error, content.to_owned()));
        }
        fn emit_stream_start(&self, _msg_id: &str) {}
        fn emit_stream_end(
            &self,
            _msg_id: &str,
            _turns: usize,
            _input_tokens: u64,
            _output_tokens: u64,
            _cache_creation_tokens: u64,
            _cache_read_tokens: u64,
        ) {
        }
        fn emit_error(&self, _msg: &str) {}
        fn emit_info(&self, _msg: &str) {}
    }

    #[test]
    fn default_sink_fails_image_generator_without_durable_delivery() {
        let sink = RecordingSink::default();
        let result = sink.emit_tool_result_with_images(
            "call-1",
            "image_gen",
            false,
            "provider said success",
            &[ToolImage {
                media_type: "image/png".into(),
                data: "bytes".into(),
            }],
        );
        assert!(matches!(result, ToolMediaDelivery::Failed { .. }));
        let emitted = sink.0.lock().unwrap();
        assert!(emitted[0].0);
        assert!(emitted[0].1.contains("Artifact delivery failed"));
    }

    #[test]
    fn default_sink_fails_text_only_artifact_generator_but_allows_screenshot_context() {
        let sink = RecordingSink::default();
        assert!(matches!(
            sink.emit_tool_result_with_images("call-1", "export_report", false, "done", &[]),
            ToolMediaDelivery::Failed { .. }
        ));
        assert!(matches!(
            sink.emit_tool_result_with_images(
                "call-2",
                "browser_screenshot",
                false,
                "screenshot",
                &[ToolImage {
                    media_type: "image/png".into(),
                    data: "bytes".into(),
                }],
            ),
            ToolMediaDelivery::Unmanaged
        ));
        assert!(matches!(
            sink.emit_tool_result_with_images(
                "call-3",
                "custom_visual_creator",
                false,
                "done",
                &[ToolImage {
                    media_type: "image/png".into(),
                    data: "bytes".into(),
                }],
            ),
            ToolMediaDelivery::Failed { .. }
        ));
    }

    #[test]
    fn artifact_expectation_uses_tool_identity_not_context_screenshot_names() {
        assert_eq!(artifact_expectation("mcp__reports__export_report__abc"), ArtifactExpectation::File);
        assert_eq!(
            artifact_expectation("mcp__openai__image_gen__abc"),
            ArtifactExpectation::Image
        );
        assert_eq!(
            artifact_expectation("mcp__speech__text_to_speech__abc"),
            ArtifactExpectation::Audio
        );
        assert_eq!(artifact_expectation("text_to_speech"), ArtifactExpectation::Audio);
        assert_eq!(artifact_expectation("render-video"), ArtifactExpectation::Video);
        assert_eq!(artifact_expectation("browser_screenshot"), ArtifactExpectation::None);
    }

    #[test]
    fn artifact_expectation_handles_camel_case_concatenated_and_plural_names() {
        assert_eq!(artifact_expectation("generateImage"), ArtifactExpectation::Image);
        assert_eq!(artifact_expectation("renderImages"), ArtifactExpectation::Image);
        assert_eq!(artifact_expectation("generateimages"), ArtifactExpectation::Image);
        assert_eq!(artifact_expectation("image_generator"), ArtifactExpectation::Image);
        assert_eq!(artifact_expectation("mcp__reports__export"), ArtifactExpectation::File);
        assert_eq!(artifact_expectation("exportPresentations"), ArtifactExpectation::File);
        assert_eq!(artifact_expectation("createArtifacts"), ArtifactExpectation::Any);
        assert_eq!(artifact_expectation("renderPng"), ArtifactExpectation::Image);
        assert_eq!(artifact_expectation("generateMp3"), ArtifactExpectation::Audio);
        assert_eq!(artifact_expectation("exportMp4"), ArtifactExpectation::Video);
        assert_eq!(artifact_expectation("exportZip"), ArtifactExpectation::File);
        assert!(is_context_only_image_tool("browserScreenshot"));
        assert!(is_context_only_image_tool("mcp__viewer__viewImages"));
    }

    #[test]
    fn artifact_contract_preserves_exact_high_signal_formats() {
        let cases = [
            ("render_png", ArtifactRequirement::Png, "image/png"),
            ("generateJpeg", ArtifactRequirement::Jpeg, "image/jpeg"),
            ("render_webp", ArtifactRequirement::Webp, "image/webp"),
            ("exportGif", ArtifactRequirement::Gif, "image/gif"),
            ("generate_mp3", ArtifactRequirement::Mp3, "audio/mpeg"),
            ("synthesizeWav", ArtifactRequirement::Wav, "audio/wav"),
            ("exportFlac", ArtifactRequirement::Flac, "audio/flac"),
            ("exportOgg", ArtifactRequirement::Ogg, "audio/ogg"),
            ("exportM4a", ArtifactRequirement::M4a, "audio/mp4"),
            ("render_mp4", ArtifactRequirement::Mp4, "video/mp4"),
            ("renderM4v", ArtifactRequirement::Mp4, "video/mp4"),
            ("exportWebm", ArtifactRequirement::Webm, "video/webm"),
            ("exportMov", ArtifactRequirement::Mov, "video/quicktime"),
            ("exportpdf", ArtifactRequirement::Pdf, "application/pdf"),
            ("exportZip", ArtifactRequirement::Zip, "application/zip"),
            (
                "exportDocx",
                ArtifactRequirement::Docx,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "exportXlsx",
                ArtifactRequirement::Xlsx,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "exportPptx",
                ArtifactRequirement::Pptx,
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("downloadCsv", ArtifactRequirement::Csv, "text/csv"),
            ("generateJson", ArtifactRequirement::Json, "application/json"),
            ("exportMarkdown", ArtifactRequirement::Markdown, "text/markdown"),
            ("saveHtml", ArtifactRequirement::Html, "text/html"),
            ("exportXml", ArtifactRequirement::Xml, "application/xml"),
        ];
        for (identity, requirement, mime) in cases {
            let contract = artifact_contract(identity).expect(identity);
            assert_eq!(contract.requirement, requirement, "{identity}");
            assert_eq!(contract.expectation, requirement.expectation(), "{identity}");
            assert!(contract.accepts_mime(mime), "{identity}");
            assert!(!contract.accepts_mime("application/octet-stream"), "{identity}");
        }
        assert!(artifact_contract("read_png").is_none());
        assert!(artifact_contract("edit_pdf").is_none());
        assert!(artifact_contract("inspect_mp4").is_none());
        assert_eq!(
            artifact_contract("export_png_jpeg").unwrap().requirement,
            ArtifactRequirement::Image
        );
    }

    #[test]
    fn common_artifact_tool_identities_are_fail_closed_and_transform_aware() {
        let cases = [
            ("draw_poster", ArtifactRequirement::Image),
            ("design_logo", ArtifactRequirement::Image),
            ("design_presentation", ArtifactRequirement::File),
            ("record_podcast", ArtifactRequirement::Audio),
            ("record_narration", ArtifactRequirement::Audio),
            ("package_zip", ArtifactRequirement::Zip),
            ("packagezip", ArtifactRequirement::Zip),
            ("package_pdf", ArtifactRequirement::File),
            ("package_images", ArtifactRequirement::File),
            ("download_csv", ArtifactRequirement::Csv),
            ("export_md", ArtifactRequirement::Markdown),
            ("save_htm", ArtifactRequirement::Html),
            ("convert_pdf_to_csv", ArtifactRequirement::Csv),
            ("convert_png_to_pdf", ArtifactRequirement::Pdf),
            ("convert_pdf", ArtifactRequirement::File),
            ("convertpdf", ArtifactRequirement::File),
        ];
        for (identity, requirement) in cases {
            assert_eq!(artifact_contract(identity).unwrap().requirement, requirement, "{identity}");
        }
        assert!(artifact_contract("read_pdf").is_none());
        assert!(artifact_contract("inspect_csv").is_none());
        assert!(artifact_contract("write_file").is_none());
        assert!(artifact_contract("package_manager").is_none());
        assert!(artifact_contract("converter_status").is_none());
        assert!(artifact_contract("create_image_decoder").is_none());
        assert!(artifact_contract("design_pdf_parser").is_none());
        assert!(artifact_contract("record_video_metadata").is_none());
        assert!(!artifact_contract("convert_pdf_to_csv").unwrap().accepts_mime("application/pdf"));
        assert!(artifact_contract("convert_pdf_to_csv").unwrap().accepts_mime("text/csv"));
        assert!(artifact_contract("convert_pdf").unwrap().accepts_mime("text/csv"));
        assert!(artifact_contract("package_pdf").unwrap().accepts_mime("application/zip"));
    }

    #[test]
    fn exact_requirement_matches_canonical_receipt_mime_only() {
        assert!(ArtifactRequirement::Png.accepts_mime(" IMAGE/PNG; charset=binary "));
        assert!(!ArtifactRequirement::Png.accepts_mime("image/jpeg"));
        assert!(!ArtifactRequirement::Jpeg.accepts_mime("image/jpg"));
        assert!(!ArtifactRequirement::Mp3.accepts_mime("audio/mp3"));
        assert!(!ArtifactRequirement::Wav.accepts_mime("audio/x-wav"));
        assert!(!ArtifactRequirement::Webp.accepts_mime("image/png"));
        assert!(!ArtifactRequirement::Mov.accepts_mime("video/mp4"));
        assert!(!ArtifactRequirement::Docx.accepts_mime("application/zip"));
        assert!(!ArtifactRequirement::Zip.accepts_mime(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
        assert_eq!(ArtifactRequirement::Pdf.label(), "PDF artifact");

        let contract = artifact_contract("render_png").unwrap();
        assert!(contract.validate_mimes(&["image/png"]).is_ok());
        assert!(contract
            .validate_mimes(&["image/png", "image/png"])
            .is_ok());
        assert!(matches!(
            contract.validate_mimes::<&str>(&[]),
            Err(ArtifactContractViolation::Count {
                expected: 1,
                actual: 0
            })
        ));
        assert!(matches!(
            contract.validate_mimes(&["image/jpeg"]),
            Err(ArtifactContractViolation::Mime {
                index: 0,
                requirement: ArtifactRequirement::Png
            })
        ));
    }

    #[test]
    fn image_contract_parses_explicit_output_count_fail_closed() {
        let contract = artifact_contract_with_input(
            "image_gen",
            &serde_json::json!({"count": 4, "num_outputs": 4}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(contract.requested_count, Some(4));
        assert_eq!(contract.expected_count(), 4);
        assert!(contract
            .validate_mimes(&["image/png", "image/jpeg", "image/webp", "image/gif"])
            .is_ok());

        for input in [
            serde_json::json!({"count": 0}),
            serde_json::json!({"n": -1}),
            serde_json::json!({"num_images": "2"}),
            serde_json::json!({"num_outputs": 2.5}),
        ] {
            assert!(matches!(
                artifact_contract_with_input("image_gen", &input),
                Err(ArtifactCountError::InvalidValue { .. })
            ));
        }
        assert!(matches!(
            artifact_contract_with_input(
                "image_gen",
                &serde_json::json!({"count": MAX_ARTIFACT_OUTPUT_COUNT + 1})
            ),
            Err(ArtifactCountError::ExceedsLimit { .. })
        ));
        assert!(matches!(
            artifact_contract_with_input(
                "image_gen",
                &serde_json::json!({"count": 2, "n": 3})
            ),
            Err(ArtifactCountError::ConflictingValues)
        ));

        let pdf = artifact_contract_with_input(
            "export_pdf",
            &serde_json::json!({"count": "unrelated exporter option"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(pdf.requested_count, None);

        let csv_batch = artifact_contract_with_input(
            "download_csv",
            &serde_json::json!({"num_files": 3}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(csv_batch.requested_count, Some(3));

        for ordinary_tool in ["Read", "Edit", "Inspect"] {
            assert_eq!(
                artifact_contract_with_input(
                    ordinary_tool,
                    &serde_json::json!({"n": 4, "count": "ordinary field"}),
                )
                .unwrap(),
                None,
                "{ordinary_tool} count-like fields must not create an artifact contract"
            );
        }
    }

    #[test]
    fn artifact_contract_merge_narrows_and_rejects_conflicts() {
        let broad = ArtifactContract {
            expectation: ArtifactExpectation::Any,
            requirement: ArtifactRequirement::Any,
            requested_count: None,
        };
        let png = ArtifactContract {
            expectation: ArtifactExpectation::Image,
            requirement: ArtifactRequirement::Png,
            requested_count: Some(2),
        };
        assert_eq!(broad.merge(png).unwrap(), png);

        let image_default = ArtifactContract {
            expectation: ArtifactExpectation::Image,
            requirement: ArtifactRequirement::Image,
            requested_count: None,
        };
        assert_eq!(image_default.merge(png).unwrap(), png);

        let jpeg = ArtifactContract {
            expectation: ArtifactExpectation::Image,
            requirement: ArtifactRequirement::Jpeg,
            requested_count: Some(2),
        };
        assert!(matches!(
            png.merge(jpeg),
            Err(ArtifactContractMergeError::Requirement { .. })
        ));

        let png_count_four = ArtifactContract {
            requested_count: Some(4),
            ..png
        };
        assert!(matches!(
            png.merge(png_count_four),
            Err(ArtifactContractMergeError::Count {
                existing: 2,
                observed: 4
            })
        ));
    }

    #[test]
    fn file_and_any_expectations_accept_media_mime_types() {
        assert!(ArtifactExpectation::File.accepts_mime("image/png"));
        assert!(ArtifactExpectation::File.accepts_mime("audio/wav"));
        assert!(ArtifactExpectation::Any.accepts_mime("application/octet-stream"));
        assert!(!ArtifactExpectation::File.accepts_mime("  "));
    }

    #[test]
    fn failed_tool_media_remains_unmanaged_diagnostic_context() {
        let sink = RecordingSink::default();
        assert!(matches!(
            sink.emit_tool_result_with_images(
                "call-failed",
                "image_gen",
                true,
                "provider failed",
                &[ToolImage {
                    media_type: "image/png".into(),
                    data: "diagnostic".into(),
                }],
            ),
            ToolMediaDelivery::Unmanaged
        ));
        let emitted = sink.0.lock().unwrap();
        assert!(emitted[0].0);
        assert_eq!(emitted[0].1, "provider failed");
    }

    #[test]
    fn test_text_truncation_short_string_unchanged() {
        let result = truncate_display("hello", 10);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_text_truncation_exact_length_unchanged() {
        let result = truncate_display("helloworld", 10);
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_text_truncation_long_string_truncated() {
        let result = truncate_display("hello world this is long", 10);
        assert_eq!(result, "hello worl...");
    }

    #[test]
    fn test_text_truncation_empty_string() {
        let result = truncate_display("", 10);
        assert_eq!(result, "");
    }

    #[test]
    fn test_turn_stats_no_panic() {
        let formatter = OutputFormatter::new(true);
        // Verify turn_stats does not panic with various inputs
        formatter.turn_stats(1, 100, 50, 0, 0);
        formatter.turn_stats(5, 1000, 500, 200, 300);
        formatter.turn_stats(0, 0, 0, 0, 0);
    }

    #[test]
    fn test_text_truncation_cjk_does_not_panic() {
        // Each CJK char is 3 bytes; byte-based slicing at max=200 would land
        // mid-character and panic without the char_indices fix.
        let cjk: String = "你好世界测试".chars().cycle().take(200).collect();
        let result = truncate_display(&cjk, 50);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_text_truncation_mixed_cjk_ascii_does_not_panic() {
        let mixed = "abc你好def世界ghi测试".repeat(20);
        let result = truncate_display(&mixed, 30);
        assert!(result.ends_with("..."));
    }
}
