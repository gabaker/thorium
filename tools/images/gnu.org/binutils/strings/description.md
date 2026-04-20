# strings (GNU Binutils)

GNU `strings` prints the sequences of printable characters found in files, making it useful for determining the contents of non-text files such as compiled binaries.

## Overview

For each file given, `strings` prints printable character sequences that are at least 4 characters long (configurable) and followed by an unprintable character. It can scan the whole file or, depending on configuration, only loadable initialized data sections; when reading an unrecognized file type or from stdin it always prints all printable sequences it finds. The tool supports selectable character encodings via `-e`: single 7-bit-byte (ASCII/ISO 8859, the default), single 8-bit-byte, and 16-/32-bit big- and little-endian (useful for wide-character strings such as UTF-16/UCS-2). This Thorium deployment runs the default single-byte (ASCII) configuration, with sibling images configured for the wide-character encodings (`strings-16be`, `strings-16le`, `strings-32be`, `strings-32le`).

## Supported File Types

- Any file / arbitrary binary data (e.g. executables, object files, libraries, firmware, and other non-text files)

## Usage

Run `strings` on any binary artifact to quickly extract embedded readable text such as error messages, symbol or function names, file paths, URLs, and other string data. Because it works on arbitrary, unrecognized, or non-text files and requires no parsing of a specific format, it is commonly one of the first steps in static analysis to get an overview of a sample's readable content. Use the wide-character encoding variants when a sample is expected to contain UTF-16/UCS-2 or other multi-byte strings that the default ASCII scan would miss.

## Documentation

[GNU Binutils: strings](https://sourceware.org/binutils/docs/binutils/strings.html)

## License

GPL-3.0-or-later — part of GNU Binutils; see the [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html).
