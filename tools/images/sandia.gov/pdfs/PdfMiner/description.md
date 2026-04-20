# PDF Miner (Custom Wrapper)

A custom Python wrapper around pdfminer.six that performs deep structural inspection of PDF documents, walking cross-reference objects to detect capabilities such as embedded JavaScript and to carve embedded streams.

## Overview

This tool parses PDF documents with pdfminer.six's PDFParser and PDFDocument classes to inspect internal PDF structures rather than extract text. It iterates over every cross-reference (xref) entry and each referenced object, flagging an `EmbeddedJavascript` capability when an object dictionary contains a `JS` key and an `EmbeddedFile` capability for any PDF stream object. Detected JavaScript and stream data are written out as individual child files (`pdf_js_stream_<id>` and `pdf_stream_<id>`) for further analysis. Results are emitted as a JSON map containing the detected `capabilities` list and the full `xrefs` object structure, and the platform auto-tags samples based on the `PdfCapabilities` key.

## Supported File Types

- PDF documents (`.pdf`)

## Usage

Run this tool on PDF files when you need structural analysis beyond plain text extraction, particularly to triage potentially malicious PDFs. It is most useful for surfacing documents that embed JavaScript or carry embedded files/streams, since those are common vectors for active or hidden content. Use the carved JavaScript and stream child files as inputs to downstream analysis of the extracted payloads.

## Documentation

Internal tool. Source: [tools/images/sandia.gov/pdfs/PdfMiner](https://github.com/cisagov/thorium/tree/main/tools/images/sandia.gov/pdfs/PdfMiner)

## License

Covered by the [Thorium license](https://github.com/cisagov/thorium/blob/main/LICENSE).
