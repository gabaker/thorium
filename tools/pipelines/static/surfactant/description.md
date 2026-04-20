# Surfactant

Generates a Software Bill of Materials (SBOM) and dependency map from submitted software using LLNL's Surfactant framework. It collects surface-level metadata from recognized binaries and records the relationships between them, without executing or decompiling the sample.

## Images Run

- **surfactant** — Runs `surfactant generate` to pull surface-level metadata (file size, vendor, version) from recognized files and emit an SBOM as JSON, including per-file software entries and "Uses"/"Contains" relationships.

## Supported File Types

Primarily compiled binaries and installers: PE (Windows executables/DLLs), ELF (Linux/Unix binaries), and MSI (Windows installer packages). Mach-O and Java files are supported with optional dependencies. Files of unrecognized types can still be included, and Surfactant can process archives (e.g. zip, tar.gz, exe installers) to capture overall sample metadata and "Contains" relationships.

## Usage

Run this pipeline on a software package, binary distribution, or directory of extracted contents when you need a component inventory or dependency map. It is well suited to software supply chain analysis, building inventories of compiled software, and identifying dependencies among PE, ELF, MSI, and other binaries. Because it only reads surface-level metadata and never executes the sample, it is appropriate for static triage where understanding component composition and inter-file relationships is the goal.
