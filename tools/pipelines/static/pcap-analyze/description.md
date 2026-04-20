# pcap-analyze

A comprehensive network traffic analysis pipeline that runs an ensemble of network security monitoring tools against a packet capture, producing protocol logs, passive asset inventory, carved transferred files, and intrusion-detection alerts.

## Images Run

- **zeek-dump** — Runs Zeek against the PCAP to emit structured ASCII and JSON protocol logs (connections, DNS, HTTP, SSL/TLS, files) and an HTML triage summary of DNS activity and connections.
- **prads** — Passively enumerates hosts and services from the capture, performing OS fingerprinting and service identification to build an asset inventory without sending packets.
- **tshark** — Summarizes the capture and carves objects transferred over HTTP, TFTP, IMF, SMB, and DICOM into child files for further analysis.
- **suricata** — Inspects the capture in offline mode against community rulesets to detect threats and suspicious activity, producing a severity-ranked alert summary, capture stats, and `SuricataAlert` tags.

## Supported File Types

Packet capture files: PCAP and PCAPNG. TShark additionally auto-detects and reads capture files with gzip, Zstandard, or LZ4 compression. Non-capture inputs will not yield meaningful results.

## Usage

Submit PCAP or PCAPNG files to this pipeline for end-to-end network traffic analysis during forensics, threat hunting, or incident response. The ensemble combines protocol logging (Zeek), passive asset enumeration (PRADS), packet-level dissection and object carving (TShark), and signature-based threat detection (Suricata) to give a complete picture of the activity within a capture. Use it when the artifact under analysis is captured network traffic rather than a standalone binary.
