# Pharos FN2Hash

FN2Hash is a tool from the CMU SEI Pharos static binary analysis framework for generating a variety of hashes and other descriptive properties for the functions in an executable program.

## Overview

FN2Hash is part of the Pharos framework developed by the Software Engineering Institute at Carnegie Mellon University, which uses the ROSE compiler infrastructure for disassembly, control flow analysis, and instruction semantics. It computes per-function hashes and descriptive properties for the functions it recovers from a binary. Like the related FN2Yara tool, its output can be used to support binary similarity analysis or to provide features for machine learning algorithms. In this deployment it runs in batch mode and emits JSON results.

## Supported File Types

- Executable programs analyzed via the Pharos/ROSE framework

## Usage

Run FN2Hash on executable programs to produce function-level hashes and descriptive properties. These features are useful when you want to compare functions across samples for binary similarity analysis, identify programs that share significant amounts of code, or generate inputs for machine-learning-based classification. It is a good fit for static triage workflows where function-level fingerprints add value beyond whole-file hashing.

## Documentation

[CMU SEI Pharos framework](https://github.com/cmu-sei/pharos)

## License

BSD license. Copyright Carnegie Mellon University (Software Engineering Institute).
