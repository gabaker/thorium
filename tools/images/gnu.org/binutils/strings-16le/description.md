# strings-16le (GNU Binutils)

A variant of GNU `strings` invoked with `-e l` to extract 16-bit little-endian (UTF-16LE / UCS-2) printable character sequences from files.

## Overview

GNU `strings` prints the printable character sequences in a file that are at least 4 characters long and followed by an unprintable character. This configuration uses the `-e l` encoding option to find 16-bit little-endian wide-character strings, which the default single-byte ASCII mode would miss. It is mainly useful for determining the contents of non-text files. This image shares the same GNU Binutils `strings` binary as the base `strings` tool, differing only in the encoding flag.

## Supported File Types

- Any file / arbitrary binary data

The tool scans any file for embedded printable sequences; if the file type is unrecognizable or input is read from stdin, it displays all printable sequences it can find.

## Usage

Run this tool when a sample may contain UTF-16LE / UCS-2 wide-character strings rather than single-byte ASCII text — a common encoding for wide character data such as API names, file and registry paths, and user-facing text. It is a useful complement to the default `strings` tool when triaging binaries whose human-readable content is stored in 16-bit little-endian form. Because it scans arbitrary binary data, it can be applied to any non-text file where wide-character strings may be present.

## Documentation

[GNU Binutils: strings](https://sourceware.org/binutils/docs/binutils/strings.html)

## License

GPL-3.0-or-later — part of GNU Binutils; see the [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html).
