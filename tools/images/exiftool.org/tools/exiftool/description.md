# ExifTool

ExifTool by Phil Harvey is a platform-independent Perl library plus command-line application for reading, writing, and editing meta information in a wide variety of files.

## Overview

ExifTool reads and extracts many different metadata formats, including EXIF, GPS, IPTC, XMP, and maker notes, from a broad range of file types. In this Thorium deployment it is run in read-only mode and emits the extracted metadata as structured JSON. Notable tags such as file type, MIME type, and (for executables) PE type, machine type, and entry point are surfaced as Thorium tags.

## Supported File Types

- A wide variety of files: ExifTool reads metadata from many image, audio, video, and document formats, among others.
- Because the wrapper passes any submitted file to ExifTool, arbitrary files are accepted; ExifTool reports the detected file type and any metadata it can parse.

## Usage

Run ExifTool whenever you want to extract embedded metadata from a sample, for example to determine the true file type and MIME type, recover timestamps, GPS coordinates, camera or device details, software/version strings, and other provenance or authorship indicators. It is a fast, low-cost first pass for almost any artifact and is especially useful for triaging documents, media files, and executables where embedded metadata can reveal origin or tooling.

## Documentation

[ExifTool homepage](https://exiftool.org/)

## License

Free software released under the same terms as Perl itself (the Perl Artistic License or the GNU General Public License). The authoritative documentation references a License section but does not include its full text in the retrieved copy, so the exact terms should be confirmed at the project homepage.
