# strings-32be (GNU Binutils)

A variant of the GNU Binutils `strings` utility configured with `-e B` to extract sequences of 32-bit big-endian encoded printable characters from files.

## Overview

`strings` prints the printable character sequences in a file that are at least 4 characters long and followed by an unprintable character. It is mainly useful for determining the contents of non-text files. This variant runs with `-e B`, which per the `-e/--encoding` option selects 32-bit big-endian character encoding, allowing it to recover wide-character strings that the default single-byte (ASCII/ISO 8859) scan would miss.

## Supported File Types

- Any file / arbitrary binary data (the tool scans printable sequences in any file)
- Object and executable files (BFD-recognized object code formats), where it can limit scanning to loadable, initialized data sections

## Usage

Run this variant during static analysis when a sample may contain 32-bit big-endian encoded character strings (for example wide-character text on big-endian platforms) that single-byte or other-width `strings` scans would not surface. It complements the default ASCII `strings` pass and other encoding-width variants to give broader coverage of human-readable content embedded in binaries. Useful for inspecting the contents of non-text files and pulling out indicators such as paths, URLs, or messages.

## Documentation

[GNU Binutils: strings](https://sourceware.org/binutils/docs/binutils/strings.html)

## License

GPL-3.0-or-later — part of GNU Binutils; see the [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html).
