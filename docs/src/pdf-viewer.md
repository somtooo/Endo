---
title: PDF Viewer
description: "Read and navigate PDF documents in Zed."
---

# PDF Viewer

Open a PDF from the project panel or with the file picker to read it in a tab.
Pages scroll continuously, and the viewer reloads the file when it changes on
disk. PDFs in remote projects are downloaded to a temporary local file for
viewing; the temporary copy is removed after it has been read.

Use {#action pdf_viewer::GoToPage} to enter a page number. The page number in
the viewer can also be edited directly. Zoom with {#action pdf_viewer::ZoomIn}
and {#action pdf_viewer::ZoomOut}, or use {#action pdf_viewer::FitToView} and
{#action pdf_viewer::ZoomToActualSize}. Actual size maps PDF points to logical
screen pixels, so its physical size depends on the display and system scaling.

Drag over text to select it and use {#action pdf_viewer::CopyDocumentText} to
copy the selection. With no selection, the same action copies text from the
whole document after text extraction finishes. Scanned pages without an embedded
text layer cannot be selected or copied as text.

The viewer displays PDFs but does not edit their contents or annotations.
