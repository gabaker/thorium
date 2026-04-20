# strings-16be (GNU Binutils)

A variant of the GNU Binutils `strings` tool configured to extract 16-bit big-endian encoded character sequences from files.

## Overview

This is the GNU Binutils `strings` program invoked with `-e b` to select the 16-bit big-endian character encoding. `strings` prints the printable character sequences (by default at least 4 characters long and followed by an unprintable character) that it finds in each file, making it useful for determining the contents of non-text files. The big-endian 16-bit encoding applies to wide-character text such as Unicode UTF-16/UCS-2 stored in big-endian byte order. It shares the same container image as the base `strings` tool.

## Supported File Types

- Any file / arbitrary binary data (object files, executables, and other non-text files)
- Most useful for files containing 16-bit big-endian (e.g. UTF-16BE / UCS-2) wide-character text

## Usage

Run on binary samples when 16-bit big-endian wide-character strings (such as UTF-16BE/UCS-2 text) are expected or suspected, where the default single-byte `strings` scan would miss them. It is mainly useful for surfacing human-readable content in non-text files, and is best used alongside the default `strings` tool and other encoding variants for comprehensive string coverage.

## Documentation

[GNU Binutils: strings](https://sourceware.org/binutils/docs/binutils/strings.html)

## License

GPL-3.0-or-later — part of GNU Binutils; see the [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html).
