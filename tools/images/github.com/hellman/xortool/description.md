# xortool

xortool is a tool for XOR analysis: it guesses the key length (based on the count of equal characters) and guesses the key (based on knowledge of the most frequent character).

## Overview

xortool analyzes data encrypted with a repeating-key XOR cipher. It statistically estimates the most probable key length, then attempts to recover the key by assuming a known most-frequent byte in each key position. It can brute-force all possible most-frequent characters (optionally limited to printable ones), filter candidate outputs against a target character set (printable, base32, base64, or a custom set), use known plaintext for decoding, and accept either raw file input or hex-encoded input.

## Supported File Types

- Any file / arbitrary binary data suspected of repeating-key XOR encryption
- Hex-encoded text representing such data (via the `-x` option)

## Usage

Run xortool on samples suspected of containing repeating-key XOR-encrypted content, such as malware payloads, configuration blocks, or C2 strings. It is most effective when the underlying plaintext has a predictable byte-frequency distribution (for example, `00` is the most frequent byte in binaries and `20` in text files), which lets the tool key off the most frequent character. Use it during static analysis to identify likely key lengths and recover candidate keys when simple single-byte XOR or visual inspection is insufficient.

## Documentation

[hellman/xortool on GitHub](https://github.com/hellman/xortool)

## License

MIT License
