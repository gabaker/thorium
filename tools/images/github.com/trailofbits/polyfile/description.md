# PolyFile

PolyFile is a utility to identify and map the semantic and syntactic structure of files, including polyglots, chimeras, and schizophrenic files.

## Overview

PolyFile ships a cleanroom, pure-Python implementation of libmagic and can act as a drop-in replacement for the `file` command, identifying all 263 MIME types libmagic recognizes. Unlike `file`, it can recursively identify embedded files (similar to binwalk) and detect polyglots: files that are simultaneously valid in multiple formats. Beyond identification, it parses and semantically maps several formats down to precise byte ranges. In this deployment it produces an SBuD JSON structure mapping, a human-readable "explain" result, and an interactive HTML hex viewer of the file.

## Supported File Types

- Any file / arbitrary binary data (libmagic-style identification across all 263 supported MIME types)
- Formats with full parsing and semantic mapping:
  - PDF
  - ZIP (including recursive identification of all ZIP contents)
  - JPEG/JFIF
  - iNES
  - Any other format described by a Kaitai Struct (KSY) grammar

## Usage

Run PolyFile to identify a file's type(s) and discover embedded, overlapping, or polyglot structures. It is especially useful when analyzing suspected polyglot or chimera files, detecting hidden content embedded within documents or archives, and understanding the precise byte-level layout of complex or malformed files. It is a strong choice whenever `file`-style identification is insufficient and you need recursive, structural insight; the generated HTML hex viewer aids detailed manual inspection.

## Documentation

[PolyFile on GitHub](https://github.com/trailofbits/polyfile)

## License

Apache-2.0. Developed by Trail of Bits. Copyright 2019, Trail of Bits.
