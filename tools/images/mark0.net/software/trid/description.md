# TrID

TrID is a utility that identifies file types from their binary signatures. Rather than relying on hard-coded rules, it uses an extensible, trainable database of definitions that describe recurring patterns for supported file types.

## Overview

TrID compares a file against a community-maintained database of definitions (TrIDDefs), each describing recurring binary patterns for a known file type, and reports a ranked list of probable matches with confidence percentages. Because the database has no fixed rules and can be extended or trained on new formats, TrID stays current as new file types appear and can even be taught to recognize custom, private formats. The definitions library is constantly expanding, and the more definitions are available, the more accurate the analysis of an unknown file becomes. In this deployment, matches scoring above 25% are emitted as `FileTypeMatch` tags.

## Supported File Types

- Any file / arbitrary binary data — TrID reads the raw bytes of any file and matches them against its definitions database.

## Usage

Run TrID on any unknown or unidentified file to determine its likely type. It is useful for triaging files received via e-mail, supporting forensic analysis, and aiding file recovery where the original type is unknown. Because its database is extensible and not limited to fixed magic numbers, it is well suited as a complementary identifier alongside other file-type detection tools, especially for formats that lack distinctive headers.

## Documentation

[TrID - File Identifier](https://mark0.net/soft-trid-e.html)

## License

Unstated. The authoritative documentation does not name a specific license for TrID.
