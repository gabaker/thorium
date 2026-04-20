# Surfactant

Surfactant is a modular framework, developed by Lawrence Livermore National Laboratory (LLNL), that gathers file information for SBOM (Software Bill of Materials) generation and dependency analysis.

## Overview

Surfactant collects information from a set of files to generate an SBOM, and can also manipulate and analyze the information contained in SBOMs. It pulls "surface-level" metadata from recognized file types contained within a directory structure corresponding to an extracted software package, without running the files or performing decompilation. The generated SBOM includes a software entry per file (with metadata such as file size, vendor, and version) along with relationships ("Uses", "Contains") between entries. Surfactant is extensible through a plugin system (for example, fuzzy hashing). In this Thorium deployment, the tool runs `surfactant generate` on the submitted sample and emits an SBOM as JSON output.

## Supported File Types

- PE (Windows executables/DLLs)
- ELF (Linux/Unix binaries)
- MSI (Windows installer packages)
- Mach-O (requires the `macho` optional dependency)
- Java files (require the `java` optional dependency)

Files of unrecognized types can still be included, and recognized types provide richer metadata. Surfactant can also process archives (e.g. zip/tar.gz/exe installers) to capture overall sample metadata and "Contains" relationships.

## Usage

Run Surfactant on a file or directory of extracted software-package contents when you need a software inventory or dependency map of the components in a binary distribution. It is well suited to software supply chain analysis, building inventories of compiled software, and identifying dependencies among PE, ELF, MSI, and other binaries. Because it only reads surface-level metadata and does not execute samples, it is appropriate for static triage of packaged software where understanding component composition and inter-file relationships is the goal.

## Documentation

[Surfactant documentation (Read the Docs)](https://surfactant.readthedocs.io/en/latest/)

## License

MIT (SPDX-License-Identifier: MIT)
