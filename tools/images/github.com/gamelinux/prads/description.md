# PRADS

PRADS (Passive Real-time Asset Detection System) passively analyzes network traffic to gather information about hosts and services without sending any packets. In this Thorium image it runs in offline mode against a supplied PCAP capture file.

## Overview

PRADS enumerates active hosts and services by passively inspecting network traffic, allowing a network to be mapped without an active scan. It performs OS fingerprinting using TCP SYN/SYNACK signatures, identifies services, and detects MAC addresses. The output is structured asset data including the IP address, VLAN, port, protocol, detected service, matching fingerprint info, TTL-based distance estimate, and discovery timestamp. This data can be used alongside an IDS/IPS for event-to-application correlation or converted (via prads2snort) into a Snort `hosts_attribute.xml` for better fragmentation-policy detection.

## Supported File Types

- PCAP / packet capture files (the image invokes `prads -r <file>` to read a saved capture)

## Usage

Run PRADS on captured network traffic (PCAP) to enumerate the hosts and services that were active, infer their operating systems, and identify detected services — all without active scanning. It is useful for building a passive network asset inventory from captured traffic during incident response or for correlating observed assets with IDS/IPS events. Provide a packet capture as input; non-capture files will not yield meaningful results.

## Documentation

[gamelinux/prads on GitHub](https://github.com/gamelinux/prads)

## License

GPL-2.0-or-later (the project states "GPL v2 or better").
