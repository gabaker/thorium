# Byte Frequency

Generates a byte frequency distribution graph and a normalized variance statistic for a submitted file to help identify high-entropy data indicative of packing, compression, or encryption.

## Images Run

- **byte-frequency** — Counts how often each of the 256 possible byte values (0x00-0xFF) occurs in the sample, computes a size-normalized variance statistic, and renders a log-scaled PNG bar chart of the distribution along with the raw per-byte counts as JSON.

## Supported File Types

Any file / arbitrary binary data. The tool reads the sample byte-by-byte and does not depend on a specific file format.

## Usage

Run this pipeline on any sample you suspect may be packed, compressed, or encrypted to check whether it exhibits high entropy, since a near-uniform byte distribution is a common indicator of obfuscation. For executables whose byte-frequency graph suggests packing, follow up with static unpackers (such as `upx-unpack`) or dynamic analysis to recover an unpacked sample. Because the variance value is normalized across file sizes, it is also useful for comparing samples and spotting outliers within a group of related files.
