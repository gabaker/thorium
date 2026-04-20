# Dump Symbols

Extracts the defined, demangled symbol table from a compiled binary using GNU Binutils `nm`, and tags whether any symbols were found so downstream steps can tell stripped binaries apart from those that retain symbol information.

## Images Run

- **dump-symbols** — Wraps GNU Binutils `nm` (invoked with `--quiet --defined-only --demangle`) to list a binary's defined symbols in human-readable form, writing them to a result file and setting a `HasSymbols: True/False` tag.

## Supported File Types

Object files and executables that GNU `nm` can parse via the BFD library, including ELF and PE (DLL) object files.

## Usage

Run this pipeline on compiled object files and executables to extract their defined symbol tables, revealing function names, global/static variables, and other defined symbols (demangled where applicable, which is especially helpful for C++ binaries). A stripped binary produces no symbols and is tagged `HasSymbols: False` — itself a useful signal that lets downstream analysis branch on whether symbol information is present.
