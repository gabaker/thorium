# Email Parser

Email Parser is a custom Rust tool that parses RFC 5322 / MIME email messages (.eml) and extracts structured metadata, headers, body content, and attachments for analysis.

## Overview

The Email Parser reads a single raw email file and uses the `mail-parser` Rust crate to extract sender, recipient, CC, and BCC addresses, the subject, the full set of message headers, and both plain-text and HTML message bodies. Attachments are decoded and written out as carved child files for downstream analysis by other pipeline tools. Results are emitted as structured JSON, and key attributes (addresses, subject, attachment names and content types, and flags such as having attachments, multipart attachments, or a text/HTML body-count mismatch) are recorded as Thorium tags.

## Supported File Types

- RFC 5322 / MIME email messages, typically `.eml` files

The tool reads the input as UTF-8 text and parses it as a standard MIME/RFC 822 message; it does not handle Outlook `.msg` (MAPI compound binary) files.

## Usage

Run the Email Parser on email files to extract addresses, subject, headers, body content, and attachments before further triage. It is useful for analyzing phishing emails, pulling indicators of compromise from email-based attacks, and processing email artifacts collected during forensic investigations. Extracted attachments are submitted as child files so they can be scanned by other tools in the pipeline.

## Documentation

[tools/images/sandia.gov/emails/email-parser](https://github.com/cisagov/thorium/tree/main/tools/images/sandia.gov/emails/email-parser)

## License

Covered by the [Thorium license](https://github.com/cisagov/thorium/blob/main/LICENSE).
