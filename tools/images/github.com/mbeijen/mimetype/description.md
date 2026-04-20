# mimetype (File::MimeInfo)

`mimetype` is the command-line utility from the Perl `File::MimeInfo` module that determines the MIME type of a file using the freedesktop.org Shared MIME-info database. It is a `file(1)` work-alike that outputs MIME types instead of human-readable descriptions.

## Overview

`mimetype` identifies file types according to the freedesktop.org Shared MIME-info specification rather than the magic database used by `file`. It combines several matching methods, including filename globs (extensions), content magic bytes, and inode type, to resolve a file's MIME type. Because it uses a different signature database and matching algorithm than other tools, it serves as an independent second opinion for file type identification. In this deployment it is invoked in brief mode (`mimetype -b`), emitting the bare MIME type, which is recorded as a `MIMEType` tag.

## Supported File Types

- Any file / arbitrary binary data

The tool inspects each provided file and reports a MIME type based on its name and content; it does not require a specific input format.

## Usage

Run `mimetype` whenever you need a MIME type identification for a sample, particularly as a complement to other file-typing tools such as `file` or Magika. Because it relies on the freedesktop.org database and the XDG specification rather than the `file` magic database, it is valuable for cross-validation when file type is ambiguous or when you do not trust a sample's name or extension. It is well suited to broad triage workflows where a quick, standards-based MIME classification of arbitrary files is needed.

## Documentation

[File-MimeInfo (File::MimeInfo Perl module)](https://github.com/michal-josef-spacek/File-MimeInfo)

## License

Free software distributed under the same terms as Perl itself (the Artistic License or the GNU GPL). The source does not name a single SPDX identifier; it states only that the program may be redistributed and modified "under the same terms as Perl."
