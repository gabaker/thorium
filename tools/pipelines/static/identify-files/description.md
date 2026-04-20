# Identify Files

An ensemble file type identification pipeline that runs multiple independent identification tools against a submitted sample and aggregates their results for high-confidence determination of what a file actually is.

## Images Run

- **exiftool** — Reads embedded metadata (EXIF, GPS, IPTC, XMP, maker notes) and surfaces detected file type, MIME type, and executable details such as PE/machine type as Thorium tags.
- **file-linux** — The standard Unix `file(1)`/libmagic command that determines a file's type by matching its contents against several thousand magic signatures.
- **magika** — Google's AI-powered, deep-learning file type detector trained on ~100M samples across 200+ content types, especially strong at classifying textual content.
- **peid** — Detects packers, compilers, and cryptors on Windows PE files using a database of 5,500+ signatures (applies only to valid PE executables).
- **mimetype** — Resolves a MIME type via the freedesktop.org Shared MIME-info database (globs, content magic, inode type), providing an independent XDG-based second opinion.
- **trid** — Identifies file types via statistical pattern matching against an extensible, community-maintained definitions database, emitting ranked matches with confidence percentages.
- **polyfile** — A pure-Python libmagic implementation that identifies file structure, recursively detects embedded files, and flags polyglots, with byte-level semantic mapping of select formats.

## Supported File Types

Any file / arbitrary binary or textual data. Most images in this pipeline (exiftool, file-linux, magika, mimetype, trid, polyfile) accept any submitted artifact and report a detected type, so the pipeline is intended for unknown or untrusted samples of any format. The exception is peid, which applies only to Windows PE executables and reports non-PE inputs as such. PolyFile additionally provides deeper structural parsing for formats such as PDF, ZIP, JPEG/JFIF, and iNES (and any format with a Kaitai Struct grammar).

## Usage

Submit any unknown, untrusted, or potentially disguised file to this pipeline to establish what it actually is before selecting more specialized analysis. The ensemble cross-validates results across distinct methods (magic numbers, AI inference, signatures, MIME databases, and structural mapping), giving higher confidence than any single tool, and is especially valuable for samples with missing, misleading, or stripped extensions. It also surfaces packer detection for PE files and reveals embedded or polyglot content, making it a strong first-pass triage step for broad file-typing workflows.
