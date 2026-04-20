# Suricata

Suricata is a network IDS, IPS, and Network Security Monitoring (NSM) engine developed by the Open Information Security Foundation (OISF) and the Suricata community.

## Overview

Suricata inspects network traffic and detects threats using a rule and signature language. It supports real-time intrusion detection (IDS), inline intrusion prevention (IPS), and network security monitoring (NSM), and is built to handle largely untrusted input at high traffic volumes. In this Thorium deployment, Suricata reads a packet capture in offline mode using community-maintained rulesets, then a summarizer parses the EVE JSON event log to produce an alert summary (signature ID, signature, severity, count, timestamps), per-capture stats, and `SuricataAlert` tags.

## Supported File Types

- PCAP / packet capture files (read in Suricata offline mode via `suricata -r`)

## Usage

Run Suricata on a PCAP to detect intrusions, malware communications, policy violations, and other suspicious network activity flagged by its rulesets. It is best suited to analyzing network captures collected during incident response or triage, surfacing alerts ranked by severity and frequency along with capture statistics. Use it when a sample or artifact under analysis is network traffic rather than a standalone binary.

## Documentation

[Suricata User Guide](https://docs.suricata.io/)

## License

GPL-2.0 — see the project's [LICENSE](https://github.com/OISF/suricata/blob/master/LICENSE).
