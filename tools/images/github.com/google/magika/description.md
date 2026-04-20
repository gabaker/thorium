# Magika

Magika is an AI-powered file type detection tool that uses deep learning to accurately identify a file's content type. It runs a custom, highly optimized model that performs precise file identification within milliseconds, even on a single CPU.

## Overview

Magika has been trained and evaluated on a dataset of approximately 100 million samples across 200+ content types, covering both binary and textual file formats, and achieves an average ~99% accuracy on its test set. It uses a per-content-type threshold system that decides whether to trust the model's prediction or fall back to a generic label (such as "Generic text document" or "Unknown binary data"), with configurable prediction modes (high-confidence, medium-confidence, best-guess). Inference time is near-constant regardless of file size, since the model only inspects a limited subset of the file's content. Magika is used at scale at Google to route Gmail, Drive, and Safe Browsing files to the appropriate scanners, and has been integrated with VirusTotal and abuse.ch.

## Supported File Types

- Any file / arbitrary binary or textual data.
- Magika recognizes 200+ specific content types spanning code (e.g. Python, C, CSS, Assembly, Dockerfile), documents (e.g. Microsoft Word), text formats (e.g. RFC 822 mail, INI configuration, CSV), and many binary formats. Inputs it cannot confidently classify are reported with generic labels such as "Unknown binary data" or "Empty file".

## Usage

Run Magika to determine the true content type of a file when its type is unknown, untrusted, or potentially disguised. It is especially valuable for accurately identifying textual content types, where traditional magic-number-based tools often struggle, and is well suited to triaging large sets of files since it processes many inputs at once with near-constant inference time regardless of file size.

## Documentation

[Magika website](https://securityresearch.google/magika)

## License

Apache-2.0 (see the project's LICENSE). This project is not an official Google project.
