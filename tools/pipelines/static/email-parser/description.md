# Email Parser

Parses email messages (.eml) to extract structured metadata, headers, body content, and attachments for forensic analysis. Decoded attachments are submitted as child files for downstream triage.

## Images Run

- **email-parser** — Custom Rust tool that parses RFC 5322 / MIME email messages, extracting sender/recipient/CC/BCC addresses, subject, full headers, and plain-text and HTML bodies, while carving out attachments as child files and recording key attributes (including flags for attachments, multipart attachments, and text/HTML body-count mismatches) as Thorium tags.

## Supported File Types

- RFC 5322 / MIME email messages, typically `.eml` files

The input is read as UTF-8 text and parsed as a standard MIME/RFC 822 message. Outlook `.msg` (MAPI compound binary) files are not supported.

## Usage

Submit email files (.eml format) to extract addresses, subject, headers, body content, and attachments before further triage. It is useful for analyzing phishing emails, pulling indicators of compromise from email-based attacks, and processing email artifacts collected during forensic investigations. Extracted attachments are submitted as child files so they can be scanned by other tools in subsequent pipelines.
