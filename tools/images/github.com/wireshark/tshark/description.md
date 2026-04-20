# TShark

TShark is a network protocol analyzer. It captures packet data from a live network or reads packets from a previously saved capture file, then prints a decoded form of those packets to standard output or writes them to a file.

## Overview

TShark is the command-line counterpart of Wireshark and uses the same dissection and capture-file reading/writing code. Its native capture format is pcapng, the same format used by Wireshark. It can detect, read, and write the same capture files Wireshark supports, automatically detecting the file format and any gzip, Zstandard, or LZ4 compression. Capture filters (pcap syntax) and display filters select which packets are captured, decoded, or written, and protocol details can be shown at any layer. In this Thorium deployment, the tool runs `capinfos` to summarize the capture and uses TShark's `--export-object` to carve objects from the HTTP, TFTP, IMF, SMB, and DICOM protocols into child files.

## Supported File Types

- pcapng (TShark's native format)
- pcap and other capture files supported by Wireshark
- Optionally gzip-, Zstandard-, or LZ4-compressed capture files (format and compression are auto-detected; no specific filename extension required)

## Usage

Run TShark on packet capture files (pcap/pcapng) to inspect network traffic at the packet level. It is appropriate when an artifact is a network capture and you need detailed protocol dissection, field extraction, or filtering by protocol or content. This deployment is especially useful for capture analysis that should yield both a summary of the capture and any objects transferred over HTTP, TFTP, IMF, SMB, or DICOM, which are carved out as child files for further analysis.

## Documentation

[TShark manual page](https://www.wireshark.org/docs/man-pages/tshark.html)

## License

GPL-2.0 — Wireshark/TShark; see the project's [COPYING](https://github.com/wireshark/wireshark/blob/master/COPYING).
