# Signify

Validates and inspects Windows Authenticode digital code signatures on PE executables and related signed file types, reporting signer details, certificate chains, countersignatures, and an overall signature validity verdict.

## Images Run

- **signify** — Python module that parses and verifies Windows Authenticode signatures, surfacing signer identity, certificate chains, countersigner (timestamp) data, certificate validity dates and hashes, and an overall validity verdict.

## Supported File Types

- PE executables (.exe, .dll, and various other Windows executables)
- MSI files (.msi)
- Catalog files (.stl and .cat)
- Any flat file that is signed through a catalog file

## Usage

Run this pipeline on PE executables, DLLs, MSI installers, and catalog files to validate their Windows Authenticode signatures and inspect the associated signing certificates. It is useful for determining whether a binary is legitimately signed, examining the certificate chain and countersignatures (timestamps), and surfacing signature anomalies in suspicious or potentially tampered files. Because Authenticode can also sign arbitrary files via external catalogs, it also applies to flat files covered by a catalog signature. In this pipeline it triggers automatically on samples tagged as .exe or .dll.
