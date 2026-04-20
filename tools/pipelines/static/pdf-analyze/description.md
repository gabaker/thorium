# pdf-analyze

Extracts readable text and content from PDF documents using pdfminer.six, making document text, layout, and embedded elements available for downstream review.

## Images Run

- **pdf2text** — Runs pdfminer.six to parse PDF documents and extract their text content (along with embedded images, fonts, layout, and structural elements).

## Supported File Types

- PDF documents (PDF-1.7 specification)

## Usage

Run this pipeline on PDF files when you need their readable text content extracted for analysis. It is useful in forensic and document-analysis workflows for pulling text out of PDFs, surfacing embedded content, and examining document layout and structure. Use it whenever a PDF's text or structural elements need to be made available for keyword searching or downstream review.
