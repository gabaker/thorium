# cwe_checker

cwe_checker is a suite of static-analysis checks that detect common bug classes such as NULL pointer dereferences and buffer overflows in binary executables. These bug classes are formally known as Common Weakness Enumerations (CWEs).

## Overview

The checks combine a range of analysis techniques, from simple heuristics to abstract-interpretation-based data-flow analysis, to help analysts quickly find potentially vulnerable code paths. cwe_checker uses Ghidra to disassemble binaries into a common intermediate representation and runs its own analyses on that IR, so it works across most CPU architectures Ghidra can disassemble (including x86, ARM, MIPS, and PPC), making it valuable for firmware analysis. Implemented checks cover CWEs such as OS command injection (CWE-78), buffer overflows (CWE-119/125/787), format strings (CWE-134), integer overflows (CWE-190), use-after-free and double-free (CWE-416/415), NULL pointer dereference (CWE-476), and use of dangerous functions (CWE-676), among others. It also has experimental support for Linux loadable kernel modules (LKMs) and bare-metal binaries.

## Supported File Types

- ELF binaries (the primary focus; Linux/Unix executables across multiple CPU architectures)
- Linux loadable kernel modules (LKMs) — experimental
- Bare-metal binaries — experimental (requires a bare-metal configuration file)

## Usage

Run cwe_checker against compiled ELF binaries to surface potential memory-safety and weakness patterns (e.g., buffer overflows, NULL pointer dereferences, use-after-free, format-string and integer-overflow issues) before or during manual review. It is especially useful for firmware and embedded-device analysis where samples target multiple architectures, since the Ghidra-based IR lets the same checks run on x86, ARM, MIPS, PPC, and more. Note that both false positives and false negatives are expected, as results stem from over-approximating static analysis.

## Documentation

[fkie-cad/cwe_checker on GitHub](https://github.com/fkie-cad/cwe_checker)

## License

LGPL-3.0-or-later (GNU Library/Lesser General Public License, version 3 or, at your option, any later version), per the project's license header.
