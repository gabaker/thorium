# tcpflow

tcpflow is a tool that captures data transmitted as part of TCP connections (flows) and reconstructs each stream into its own file for convenient protocol analysis, debugging, and network forensics.

## Overview

tcpflow understands TCP sequence numbers and correctly reassembles data streams regardless of retransmissions or out-of-order delivery, storing each flow in a separate file (typically one per direction). In this Thorium deployment it processes a stored packet capture and emits a DFXML summary report (report.xml) describing every TCP flow, including source and destination IP addresses and ports, byte and packet counts, and optional MD5 hashes; reconstructed flow files are carved out as children. A common use is revealing the contents of HTTP sessions, allowing reconstruction of downloaded web content. Note that tcpflow does not understand IP fragments, so flows containing IP fragments are not recorded correctly.

## Supported File Types

- Stored packet capture files (libpcap / tcpdump-format captures, e.g. PCAP)

## Usage

Run tcpflow on captured network traffic (PCAP files) when you need to reconstruct the actual data streams of TCP connections rather than inspect individual packets. It is well suited to network forensics: extracting files or malware transferred over HTTP, analyzing application-layer protocol exchanges, recovering content from captures, and examining many TCP connections in context. It is not appropriate for non-TCP traffic or captures dominated by IP fragments.

## Documentation

[tcpflow on GitHub](https://github.com/simsong/tcpflow)

## License

GPL-3.0 — see the project's [COPYING](https://github.com/simsong/tcpflow/blob/master/COPYING).
