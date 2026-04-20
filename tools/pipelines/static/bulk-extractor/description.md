# bulk-extractor

A high-performance digital forensics pipeline that scans raw input data to extract structured artifacts such as email addresses, credit card numbers, URLs, JPEGs, and JSON snippets, without parsing file system structures.

## Images Run

- **bulk-extractor** — Probes every byte of the sample to carve and extract structured features, recursively decompressing and decoding content (e.g. BASE64-encoded JPEGs, compressed JSON) to surface artifacts that traditional carving tools miss.

## Supported File Types

`bulk_extractor` operates on raw bytes and does not depend on file system structure, so it accepts essentially any input: disk images, memory dumps, archives, packet captures (PCAP), individual files of any type, and arbitrary binary blobs. It will not work on samples that have been encrypted.

## Usage

Run this pipeline to triage a sample for embedded, encoded, or compressed artifacts without first reconstructing its file system. It is well suited to disk images, memory dumps, archives, and any opaque binary where you suspect hidden structured data such as email addresses, URLs, credit card numbers, phone numbers, GPS coordinates, EXIF metadata, or carved JPEGs and JSON. Because it recursively decodes compressed and BASE64-encoded streams, it is especially valuable for surfacing evidence that signature- or filesystem-based carvers would overlook.
