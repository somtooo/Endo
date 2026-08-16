//! Bibliography-backed citations for Markdown notes.

mod bibliography;
mod citation_picker;
mod open_source;
mod render;

pub use open_source::OpenSource;

pub use citation_picker::InsertCitation;

pub fn init(cx: &mut gpui::App) {
    citation_picker::init(cx);
    open_source::init(cx);
}

pub use bibliography::{
    BibEntry, Bibliography, CitationCompletionProvider, CitationSemanticsProvider, citation_key_at,
    citation_key_start,
};
pub const DEFAULT_LIBRARY_PATH: &str = "refs/refs.bib";
pub use render::{
    CiteForm, CiteItem, CiteSuffix, CslStyle, DEFAULT_STYLE, Locator, ParsedStyle,
    RenderedDocument, RenderedReference, ResolvedStyle, STYLE_LIST_URL, StyleFileState, StyleFiles,
    StyleProblem, StyleSource, document_style, document_style_source, parse_cite_suffix,
    render_bibliography, render_document, render_reference, resolve_bundled, resolve_style,
    resolve_style_readonly, style_file_candidates, style_from_xml, style_named, style_names,
    style_or_default, style_xml,
};

/// The folders a note's relative paths (a `csl:` file) resolve against: the
/// note's own folder, then its worktree root.
pub fn style_search_dirs(buffer: &language::Buffer, cx: &gpui::App) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    let Some(file) = buffer.file() else {
        return dirs;
    };
    let Some(local) = file.as_local() else {
        return dirs;
    };
    let absolute = local.abs_path(cx);
    if let Some(parent) = absolute.parent() {
        dirs.push(parent.to_path_buf());
    }
    // The worktree root is the absolute path with the worktree-relative one
    // peeled off.
    let depth = file.path().components().count();
    let mut root = absolute.as_path();
    for _ in 0..depth {
        match root.parent() {
            Some(parent) => root = parent,
            None => break,
        }
    }
    if !dirs.iter().any(|dir| dir == root) {
        dirs.push(root.to_path_buf());
    }
    dirs
}

pub(crate) const MARKDOWN: &str = "Markdown";

/// User-facing settings, read from the `citations` section of settings.json.
#[derive(Clone, Debug, settings::RegisterSetting)]
pub struct CitationsSettings {
    /// The BibLaTeX file inserted citations are recorded in, relative to the
    /// project root.
    pub library: String,
}

impl settings::Settings for CitationsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.citations.clone().unwrap_or_default();
        Self {
            library: content
                .library
                .filter(|library| !library.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_LIBRARY_PATH.to_string()),
        }
    }
}
