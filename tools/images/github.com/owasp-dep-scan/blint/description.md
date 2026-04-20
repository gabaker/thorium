# BLint

BLint is a Binary Linter that checks the security properties and capabilities of executables. It is powered by LIEF and can generate a CycloneDX Software Bill-of-Materials (SBOM) for supported binaries.

## Overview

BLint lets reverse engineers, security analysts, and developers quickly assess the security posture and composition of a binary. It automatically checks for common security mitigations (PIE, ASLR, NX, Stack Canaries, RELRO), performs capability analysis by reviewing imported functions and symbols (e.g. networking, filesystem, crypto usage), and extracts detailed metadata such as headers, symbols, functions, dependencies, and signature info into JSON. It can generate CycloneDX SBOMs for binaries built with Go, Rust, .NET, and Android toolchains, and can suggest functions worth targeting for fuzzing. An optional disassembler supports AArch64, x86/x86-64, ARM, Mips, and MicroMips.

## Supported File Types

- ELF (GNU and musl libc)
- PE (Windows executables and DLLs)
- Mach-O (macOS and iOS, x64 and arm64)
- WASM (WebAssembly modules)
- Android (APK, AAB; DEX files in deep mode)

## Usage

Run BLint on compiled executables and application packages when you need to audit a binary's security mitigations, enumerate its capabilities, or produce an SBOM of its third-party components. It is well suited to triaging unknown or statically-linked binaries (common with Go, Rust, and .NET), verifying that builds are hardened, inventorying components for vulnerability management, and identifying parsing-style functions as fuzzing candidates.

## Documentation

[OWASP BLint on GitHub](https://github.com/owasp-dep-scan/blint)

## License

MIT — see the project's [LICENSE](https://github.com/owasp-dep-scan/blint/blob/main/LICENSE).
