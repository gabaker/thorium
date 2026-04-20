# Pharos OOAnalyzer

OOAnalyzer is a tool from the CMU SEI Pharos static binary analysis framework for the analysis and recovery of object-oriented constructs from compiled executables.

## Overview

OOAnalyzer is part of the Pharos framework developed by the Software Engineering Institute at Carnegie Mellon University. It identifies object members and methods by tracking object pointers between functions in the program, using Prolog rules to recover object attributes. The tool was the subject of the paper "Using Logic Programming to Recover C++ Classes and Methods from Compiled Executables," published at the ACM Conference on Computer and Communications Security in 2018. Recovered OO information can be imported into the Ghidra reverse engineering tool via the Kaiju plugin.

## Supported File Types

- 32-bit x86 executables compiled by Microsoft Visual C++ (MSVC)

The current version of the tool only supports analysis of these binaries.

## Usage

Run OOAnalyzer on 32-bit Windows executables compiled with MSVC to recover C++ class structure, including object members and the methods associated with each object. This is valuable for understanding the architecture of complex C++ programs during reverse engineering, especially when source code is unavailable. It is best applied to confirmed 32-bit MSVC binaries, as other architectures and compilers are not supported.

## Documentation

[Pharos framework on GitHub](https://github.com/cmu-sei/pharos)

## License

BSD license. Copyright Carnegie Mellon University.
