# Analysis Report

## Executive Summary

A short, high-level overview of what the binary is, what it does, and the most important takeaways for a reader with limited time. Include a ascii art diagram showing this binaries behavior.

## Binary Overview

The basic facts about the analyzed binary: file type, architecture, notable metadata, and the overall structure of the program. When attributing the binary to a named malware family, cite the attribution evidence and treat other vendor names for the same family as aliases of one family ("X, also tracked as Y") — vendor ecosystems alias heavily, so list two names as distinct families only when the evidence in this analysis actually distinguishes them.

## Key Capabilities & Findings

The significant capabilities and behaviors discovered during analysis, each with the concrete evidence supporting it (function names, addresses, strings, or decompiled logic). Include ascii art diagrams describing how capabilities work. These diagrams can contain disassembly blocks if helpful to explain things.

## Network & I/O Indicators

Any network endpoints, protocols, file paths, registry keys, or other observable indicators the binary reads from or writes to.

## Artifact Inventory

A completeness contract: every concrete artifact established during the
analysis MUST appear in this section verbatim, even if already mentioned
elsewhere — decoded or decrypted strings (including status, error, operator,
and format strings), C2 URLs/domains/IPs, cryptographic keys and constants,
file and registry paths, mutexes and other named objects, command verbs and
dispatch IDs/opcodes, protocol field names, and build/version identifiers.
Organize as one markdown table per category (e.g. Artifact | Source | Meaning).
Copy each value exactly as decoded; never paraphrase, abbreviate, or summarize
away a concrete value.

## Risk Assessment

An assessment of how dangerous or suspicious this binary is, what it would mean to find it on a system, and the confidence behind that assessment.

## Open Questions & Caveats

Anything the analysis could not resolve, areas that deserve deeper investigation, and the limits of the evidence gathered.
