# cwe-checker

Runs static analysis over binary executables to detect common bug classes (CWEs) such as buffer overflows, NULL pointer dereferences, and use-after-free vulnerabilities. It surfaces potentially vulnerable code paths to support security review of compiled binaries and firmware.

## Images Run

- **cwe-checker** — Suite of static-analysis checks (from simple heuristics to abstract-interpretation-based data-flow analysis) that uses Ghidra to disassemble binaries into a common IR and flags CWEs across multiple CPU architectures (x86, ARM, MIPS, PPC).

## Supported File Types

- ELF binaries (the primary focus; Linux/Unix executables across multiple CPU architectures)
- Linux loadable kernel modules (LKMs) — experimental
- Bare-metal binaries — experimental (requires a bare-metal configuration file)

## Usage

Run this pipeline against compiled ELF binaries to surface potential memory-safety and weakness patterns (e.g., buffer overflows, NULL pointer dereferences, use-after-free, format-string, and integer-overflow issues) before or during manual review. It is especially useful for firmware and embedded-device analysis where samples target multiple architectures, since the Ghidra-based IR lets the same checks run on x86, ARM, MIPS, PPC, and more. Note that both false positives and false negatives are expected, as results stem from over-approximating static analysis.
