# strings-32le (GNU Binutils)

A variant of GNU `strings` configured with `-e L` to extract 32-bit little-endian encoded printable character sequences from files.

## Overview

GNU `strings` prints the printable character sequences in a file that are at least 4 characters long (by default) and followed by an unprintable character. This image runs `strings` with the `-e L` encoding option, which selects 32-bit little-endian characters and is useful for finding wide character strings that single-byte scanning would miss. It shares the same container image as the base `strings` tool and is mainly useful for determining the contents of non-text files.

## Supported File Types

- Any file / arbitrary binary data (e.g. object files and other non-text binaries)

## Usage

Run this variant on binary samples to recover 32-bit little-endian (wide character) strings that the default single-byte encoding scan does not surface. It is most relevant during static analysis when a sample may store text in 32-bit wide character form, ensuring coverage of that encoding width alongside the other `strings` encoding variants.

## Documentation

[GNU Binutils strings documentation](https://sourceware.org/binutils/docs/binutils/strings.html)

## License

GPL-3.0-or-later — part of GNU Binutils; see the [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html).
