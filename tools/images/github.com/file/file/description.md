# file

`file` is Ian Darwin's standard Unix file type identification command. It determines a file's type by examining its contents against a database of magic signatures rather than relying on the file name or extension.

## Overview

This is Release 5.x of the `file(1)` command, the standard "file" implementation for Linux, *BSD, and other systems. It runs filesystem tests, magic-number ("magic") tests, and language tests, and knows the magic numbers of several thousand file types. The 5.x line adds CDF (Microsoft Compound Document File) parsing, indirect magic, name/use recursion, and overhauled MIME and ASCII encoding handling. The underlying engine, `libmagic`, can also be embedded in third-party programs to identify file types without forking and exec-ing the command. In this image the tool is invoked as `file -b` to emit just the type description.

## Supported File Types

- Any file / arbitrary binary data — `file` accepts any artifact and reports its detected type.
- Recognizes several thousand formats via magic signatures, including executables (e.g. ELF), archives (e.g. tar), Microsoft Compound Document Files, CSV (RFC 4180), JSON (RFC 8259), SIMH tape files, DER-encoded data, and many document, image, and text/encoding types.

## Usage

Run `file` as one of the first steps when analyzing an unknown or untrusted artifact, to establish what format it actually is before selecting more specialized analysis tools. Because identification is based on content signatures rather than file names, it is especially valuable for samples with missing, misleading, or stripped extensions. It is broadly applicable since it operates on any binary or arbitrary file.

## Documentation

[file/file on GitHub](https://github.com/file/file)

## License

The README describes the program as "(copyright but distributable)" — Ian Darwin's freeware BSD-style license. It does not state a formal SPDX identifier in this document; see the project's COPYING file for the exact terms.
