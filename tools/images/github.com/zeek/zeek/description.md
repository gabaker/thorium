# Zeek

Zeek (formerly Bro) is a powerful framework for network traffic analysis and security monitoring. In this image it processes a packet capture and emits structured, protocol-level logs of the network activity it observes.

## Overview

Zeek ships with analyzers for many protocols, enabling high-level semantic analysis at the application layer, and keeps extensive application-layer state to provide a high-level archive of a network's activity. Its domain-specific scripting language allows site-specific monitoring policies, so it is not tied to any single detection approach. This image runs Zeek against a supplied PCAP and produces both ASCII and JSON log sets (connections, DNS, HTTP, SSL/TLS, and more), bundled into a `.tar.gz` archive. It also renders an HTML summary highlighting the top/bottom DNS queries and a preview of observed connections.

## Supported File Types

- Packet capture files (PCAP) read offline via `zeek -r`

## Usage

Run this tool on captured network traffic (PCAP) when you need to turn raw packets into structured, human- and machine-readable protocol logs. The resulting connection, DNS, HTTP, SSL/TLS and other logs are valuable for network forensics, threat hunting, and incident response, and the JSON output can be fed directly into downstream parsers or analytics. The HTML summary gives a quick triage view of DNS activity and connections without leaving the analysis interface.

## Documentation

[Zeek Documentation](https://docs.zeek.org/en/stable/index.html)

## License

BSD license (BSD-3-Clause). The Zeek README states it comes with a BSD license; see the project's COPYING file.
