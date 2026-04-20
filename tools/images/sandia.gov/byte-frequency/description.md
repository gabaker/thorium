# Byte Frequency

Byte Frequency is an internal Thorium analysis tool that counts how often each of the 256 possible byte values (0x00-0xFF) occurs in a file and renders the distribution as a frequency graph.

## Overview

The tool tallies the occurrence count of every byte value in a sample and computes a single variance statistic from the per-byte percentages, normalized so it can be compared across files of different sizes. It produces a log-scaled bar chart of the byte distribution as a PNG and returns the raw per-byte counts plus the variance value as JSON results. A flat, uniform distribution (low variance) tends to indicate encrypted or compressed/high-entropy data, while distinctive byte patterns can reflect a file's format or encoding.

## Supported File Types

- Any file / arbitrary binary data (the tool reads the sample byte-by-byte and does not depend on a specific format)

## Usage

Run this tool on any sample you suspect may be packed, compressed, or encrypted to check whether it exhibits high entropy, since a near-uniform byte distribution is a common indicator of obfuscation. For executables whose byte-frequency graph suggests packing, follow up with static unpackers (such as `upx-unpack`) or dynamic analysis to recover an unpacked sample. The variance value is comparable across samples, so it is useful for spotting outliers within a group of related files.

## Documentation

Internal tool. Source: [tools/images/sandia.gov/byte-frequency](https://github.com/cisagov/thorium/tree/main/tools/images/sandia.gov/byte-frequency)

## License

Covered by the [Thorium license](https://github.com/cisagov/thorium/blob/main/LICENSE).
