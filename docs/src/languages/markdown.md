---
title: Markdown
description: "Configure Markdown language support in Zed, including language servers, formatting, and debugging."
---

# Markdown

Markdown support is available natively in Zed.

- Tree-sitter: [tree-sitter-markdown](https://github.com/tree-sitter-grammars/tree-sitter-markdown)
- Language Server: [markdown-oxide](https://github.com/Feel-ix-343/markdown-oxide)

## Live Preview {#live-preview}

Markdown renders directly in the editor. Move the cursor into formatted text to
reveal its source; tables, images, and frontmatter have a source button. Use
{#action markdown::ToggleLivePreview} or the source button beside the preview
control to switch the current editor to raw Markdown. Set
`markdown_live_preview.enabled` to `false` to disable live preview globally.

The preview supports editable tables and properties, resizable images, task
checkboxes, math, Mermaid diagrams, wikilinks, note embeds, callouts, tags,
highlights, and footnotes. Image and file attachments can be pasted or dropped
into a saved local note; `markdown_live_preview.attachments_folder` controls
where copies are stored. Live decorations are not applied to multi-buffer or
read-only editors, or while a diff hunk is expanded.

Use **Document > Format** to toggle text formatting or headings. A word count
appears in the status bar for Markdown notes; set
`markdown_live_preview.word_count` to `false` to hide it.

## Wikilinks and Citations {#wikilinks-and-citations}

The built-in markdown-oxide language server provides wikilink completion,
references, and diagnostics. A `markdown-oxide` executable on your PATH is
used when available; otherwise the editor downloads a pinned release on first
use. Disable it with `languages.Markdown.language_servers: ["!markdown-oxide"]`.

Bibliography files in the project provide citation completion and formatted
references. Use {#action citations::InsertCitation} to search the project
bibliography and insert a citation. Set `citations.library` to change the
project-relative bibliography path used for export; the default is
`refs/refs.bib`.

## Export {#export}

Use the `markdown_export::ExportToPdf`, `markdown_export::ExportToDocx`,
`markdown_export::ExportToHtml`, or `markdown_export::ExportToEpub` action to
export the current note. The editor includes unsaved changes and expands note
embeds before handing the document to Pandoc. A missing Pandoc or Typst (used
for PDFs) is offered for download before the first export. An existing tool on
PATH is preferred. Export currently requires a saved local note.

## Syntax Highlighting Code Blocks

Zed supports language-specific syntax highlighting of markdown code blocks by leveraging [tree-sitter language grammars](../extensions/languages.md#grammar). All [Zed supported languages](../languages.md), including those provided by official or community extensions, are available for use in markdown code blocks. All you need to do is provide a language name after the opening <kbd>```</kbd> code fence like so:

````python
```python
import functools as ft

@ft.lru_cache(maxsize=500)
def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)
```
````

## Configuration

### Format

Zed supports using Prettier to automatically re-format Markdown documents. You can trigger this manually via the {#action editor::Format} action or via the {#kb editor::Format} keyboard shortcut. Alternately, you can enable format on save.

Configure formatting in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "format_on_save": "on"
    }
  },
```

### List Continuation

Zed automatically continues lists when you press Enter at the end of a list item. Supported list types:

- Unordered lists (`-`, `*`, or `+` markers)
- Ordered lists (numbers are auto-incremented)
- Task lists (`- [ ]` and `- [x]`)

Pressing Enter on an empty list item removes the marker and exits the list.

To disable this behavior, configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "extend_list_on_newline": false
    }
  },
```

### List Indentation

Zed indents list items when you press Tab while the cursor is on a line containing only a list marker. This allows you to quickly create nested lists.

To disable this behavior, configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "indent_list_on_tab": false
    }
  },
```

### Trailing Whitespace

By default Zed will remove trailing whitespace on save. If you rely on invisible trailing whitespace being converted to `<br />` in Markdown files you can disable this behavior.

Configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or add to your settings file:

```json [settings]
  "languages": {
    "Markdown": {
      "remove_trailing_whitespace_on_save": false
    }
  },
```
