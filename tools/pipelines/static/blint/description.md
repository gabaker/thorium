# BLint Pipeline

Audits the security properties and capabilities of submitted executables and generates a CycloneDX Software Bill-of-Materials (SBOM) using the BLint Binary Linter.

## Images Run

- **blint** — Binary Linter powered by LIEF that checks security mitigations (PIE, ASLR, NX, Stack Canaries, RELRO), performs capability analysis from imported functions and symbols, extracts binary metadata, and generates CycloneDX SBOMs for supported binaries.

## Supported File Types

- ELF (GNU and musl libc)
- PE (Windows executables and DLLs)
- Mach-O (macOS and iOS, x64 and arm64)
- WASM (WebAssembly modules)
- Android (APK, AAB; DEX files in deep mode)

## Usage

Run this pipeline on compiled executables and application packages when you need to audit a binary's security mitigations, enumerate its capabilities (e.g. networking, filesystem, crypto usage), or produce an SBOM of its third-party components. It is well suited to triaging unknown or statically-linked binaries (common with Go, Rust, and .NET), verifying that builds are hardened, and inventorying components for vulnerability management.
