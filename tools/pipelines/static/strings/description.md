# Strings

This pipeline scans a submitted file for embedded strings using an ensemble of tools, covering multiple character encodings (ASCII, UTF-16/UTF-32 in big- and little-endian), obfuscated and deobfuscated string recovery, and machine-learning relevance ranking.

## Images Run

- **xorstrings** — Searches the file for strings hidden with simple XOR, ROL, or SHIFT encodings by trying every single-byte key and reporting which combinations reveal printable strings.
- **floss** — FLARE Obfuscated String Solver; uses static analysis to extract static (ASCII/UTF-16LE), stack, tight, and decoded strings from executables, including Go and Rust binaries.
- **stringsifter** — Mandiant ML tool that ranks extracted strings by relevance for malware analysis so the most interesting strings surface first.
- **quantumstrand** — Experimental FLARE tool that augments PE string output with structural context (sections, import table), muting noise and highlighting suspicious entries.
- **strings** — GNU Binutils `strings` in its default single-byte (ASCII) configuration, extracting printable character sequences from any file.
- **strings-16be** — GNU `strings` configured (`-e b`) for 16-bit big-endian wide-character text (e.g. UTF-16BE/UCS-2).
- **strings-16le** — GNU `strings` configured (`-e l`) for 16-bit little-endian wide-character text (e.g. UTF-16LE/UCS-2), common in Windows binaries.
- **strings-32be** — GNU `strings` configured (`-e B`) for 32-bit big-endian wide-character text.
- **strings-32le** — GNU `strings` configured (`-e L`) for 32-bit little-endian wide-character text.

## Supported File Types

Any file / arbitrary binary data. Most of the tools (xorstrings, the GNU `strings` variants, and stringsifter) operate on raw bytes of any file, while FLOSS targets executable binaries (including Go and Rust programs) and QUANTUMSTRAND is specific to PE (Windows executable) files. Submitting executables and other binary artifacts yields the broadest coverage, but the pipeline can be run against any file.

## Usage

Run this pipeline during static analysis or triage of any binary or suspicious sample when you want comprehensive string extraction in a single pass. The ensemble guarantees strings are recovered across encodings (ASCII, UTF-16, UTF-32, big/little endian) and obfuscation methods (XOR, ROL, SHIFT, stack-constructed, in-function decoded), and that the most relevant results are surfaced via ML ranking and PE-aware context. It is especially valuable for executables and suspected malware, where indicators such as C2 domains, file paths, and configuration values are often hidden from a basic `strings` scan.
