# ssdeep

Computes a context triggered piecewise (fuzzy) hash of a submitted sample so it can be compared against other samples for similarity rather than exact equality.

## Images Run

- **ssdeep** — Runs `ssdeep` on the submitted file to generate its fuzzy hash (CTPH) signature, recorded as a string result, enabling match scoring from 0 (no match) to 100 (identical).

## Supported File Types

Any file / arbitrary binary data. ssdeep hashes the raw bytes of a file regardless of format, so no specific input type is required.

## Usage

Run this pipeline on a sample to obtain its fuzzy hash for similarity comparison against other samples. It is useful for finding near-identical or partially overlapping files, such as malware family clustering, identifying code reuse, and detecting modified variants of a known artifact, where exact-match cryptographic hashes would not reveal the relationship.
