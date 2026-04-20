# PDF Preview

A custom Rust-based tool that renders each page of a PDF document into a PNG image for safe visual preview and analysis.

## Overview

PDF Preview reads a PDF file and uses the hayro PDF rendering library to rasterize every page into a separate PNG image. It supports a configurable render scale factor (default 1.0x) and writes one PNG file per page (named `rendered_0.png`, `rendered_1.png`, ...). This produces a static visual representation of the document, allowing its appearance to be inspected without opening it in a full PDF reader or executing any embedded content.

## Supported File Types

- PDF documents (`.pdf`)

## Usage

Run this tool on PDF files to generate PNG preview images of each page. It is useful for safely previewing PDF content during malware analysis, document forensics, or any time a visual inspection is needed without rendering the document in an interactive reader. Because it only rasterizes pages to images, it lets analysts review potentially malicious PDFs without triggering embedded scripts or active content.

## Documentation

Internal tool. Source: [tools/images/sandia.gov/pdfs/PdfPreview](https://github.com/cisagov/thorium/tree/main/tools/images/sandia.gov/pdfs/PdfPreview)

## License

Covered by the [Thorium license](https://github.com/cisagov/thorium/blob/main/LICENSE).
