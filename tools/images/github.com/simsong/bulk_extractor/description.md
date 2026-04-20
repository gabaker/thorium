# bulk_extractor

`bulk_extractor` is a high-performance digital forensics exploitation tool. It is a "get evidence" button that rapidly scans any kind of input and extracts structured information such as email addresses, credit card numbers, JPEGs, and JSON snippets without parsing the file system or file system structures.

## Overview

The extracted features are stored in text files that are easily inspected, searched, or used as inputs for other forensic processing. `bulk_extractor` also creates histograms of certain kinds of features it finds, such as Google search terms and email addresses, which prior research has shown are especially useful in investigative and law enforcement applications. Unlike other digital forensics tools, it probes every byte of data to see if it is the start of a sequence that can be decompressed or otherwise decoded; when so, the decoded data are recursively re-examined. As a result, it can find artifacts like BASE64-encoded JPEGs and compressed JSON objects that traditional carving tools miss.

## Supported File Types

`bulk_extractor` does not depend on file system structure and operates on raw bytes, so it accepts essentially any input:

- Disk images and memory dumps
- Individual files of any type / arbitrary binary data
- Directories of files

## Usage

Run `bulk_extractor` when you need to triage a sample for embedded, encoded, or compressed artifacts without first reconstructing its file system. It is well suited to disk images, memory dumps, archives, packet captures, and any opaque binary blob where you suspect hidden structured data such as email addresses, URLs, credit card numbers, phone numbers, GPS coordinates, EXIF metadata, or carved JPEGs and JSON. Because it recursively decodes compressed and BASE64-encoded streams, it is especially valuable for surfacing evidence that signature- or filesystem-based carvers would overlook.

## Documentation

[bulk_extractor on GitHub](https://github.com/simsong/bulk_extractor)

## License

MIT, with portions in the public domain as a US Government work; see the project's [LICENSE](https://github.com/simsong/bulk_extractor/blob/main/LICENSE.md).
