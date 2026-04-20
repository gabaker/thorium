# Signify

Signify, a portmanteau of *signature* and *verify*, is a Python module that validates and inspects digital code signatures used to verify the authenticity and integrity of executable code. It currently verifies Windows Authenticode signatures, the technology Windows uses to confirm software integrity and identify who published a piece of software.

## Overview

Signify provides assurance about who signed a piece of software and whether it has been altered since signing. It is intended for malware analysts and security professionals to validate Authenticode signatures outside their normal ecosystem and to closely inspect the available signature data. Authenticode signatures may be embedded directly in a file without altering its functionality, or supplied externally via Authenticode catalog (.cat) files, which allows virtually any file to be signed. In this Thorium deployment, the tool parses signed data and reports signer details, certificate chains, countersigner information, certificate validity dates and hashes, and an overall signature validity verdict.

## Supported File Types

- PE executables (.exe, .dll, and various other Windows executables)
- MSI files (.msi)
- Catalog files (.stl and .cat)
- Any flat file that is signed through a catalog file

## Usage

Run Signify on PE executables, DLLs, MSI installers, and catalog files to validate their Windows Authenticode digital signatures and inspect the associated certificates. It is useful for determining whether a binary is legitimately signed, examining the signing certificate chain and countersignatures (timestamps), and surfacing signature anomalies in suspicious or potentially tampered files. Because Authenticode can also sign arbitrary files via external catalogs, it is applicable to flat files covered by a catalog signature.

## Documentation

[Signify documentation](https://signify.readthedocs.io/en/latest/)

## License

MIT — the derivative work is MIT-licensed (Copyright 2018 Ralph Broenink); see the project's [LICENSE](https://github.com/ralphje/signify/blob/master/LICENSE).
