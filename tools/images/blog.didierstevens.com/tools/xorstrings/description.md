# XORStrings

XORStrings searches a binary file for strings that have been hidden using simple XOR, ROL, or SHIFT bitwise encodings, trying every possible key and reporting which combinations reveal printable strings.

## Overview

XORStrings is a tool by Didier Stevens, best described as the combination of his XORSearch tool and the well-known strings command. It applies every single-byte XOR key (0x00-0xFF), every ROL rotation (1-7 bits), and every SHIFT operation (1-7 bits) to the entire file, then runs a strings analysis on each decoded result. For each operation/key combination it reports the number of strings found, the average string length, and the maximum string length, with output sorted by string count (or by maximum length). In this deployment the tool is run with the dump option, so it also emits the longest string recovered for each operation and key.

## Supported File Types

- Any file / arbitrary binary data (executables, documents, and other binary or data files). The file must fit in memory.

## Usage

Run XORStrings on suspicious binaries, executables, or data files when you suspect strings such as URLs, commands, or other indicators have been obfuscated with simple single-byte XOR, ROL, or SHIFT encoding, a common malware technique for evading basic static analysis. The ranked report quickly highlights the operation/key combinations that produce the most (or longest) printable strings, helping an analyst identify the correct decoding parameters and read the recovered content. It is most useful as an early triage step before deeper reverse engineering.

## Documentation

[XORSearch & XORStrings (Didier Stevens)](https://blog.didierstevens.com/programs/xorsearch/)

## License

Public Domain. The source code was put in the public domain by Didier Stevens, with no copyright.
