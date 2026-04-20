# pdfminer.six (pdf2text)

pdfminer.six is a community-maintained fork of PDFMiner, a tool for extracting information from PDF documents. It focuses on getting and analyzing text data, extracting text directly from the source code of the PDF.

## Overview

pdfminer.six can parse, analyze, and convert PDF documents, extracting content as text, images, HTML, or hOCR. It supports the PDF-1.7 specification, CJK languages and vertical writing, various font types (Type1, TrueType, Type3, CID), embedded image extraction (JPG, PNG, TIFF, JBIG2, bitmaps), multiple compression decodings (ASCIIHex, ASCII85, LZW, Flate, RunLength, CCITTFax), RC4 and AES encryption, AcroForm interactive form extraction, table of contents and tagged contents extraction, and automatic layout analysis. It can also report the exact location, font, and color of text. It is written entirely in Python.

## Supported File Types

- PDF documents (PDF-1.7 specification)

## Usage

Run pdf2text on PDF files to extract their readable text content for analysis. It is useful in forensic and document-analysis workflows for pulling text out of PDFs, surfacing embedded content, and examining document structure. Use it whenever a PDF needs its text, layout, or structural elements made available for downstream review.

## Documentation

[pdfminer.six on GitHub](https://github.com/pdfminer/pdfminer.six)

## License

MIT — see the project's [LICENSE](https://github.com/pdfminer/pdfminer.six/blob/master/LICENSE).
