# dump-symbols (GNU Binutils nm)

dump-symbols uses GNU `nm` to list the symbols from object files, writing the defined, demangled symbol names to a result file and tagging whether any symbols were found.

## Overview

This tool wraps the GNU Binutils `nm` command, which lists the symbols contained in object files. In this deployment it is invoked with `--quiet --defined-only --demangle`: `--defined-only` restricts output to symbols the file actually defines (excluding undefined/external references), `--demangle` converts low-level (e.g. C++) mangled names into human-readable form, and `--quiet` suppresses extra per-file output. For each symbol, `nm` reports its value, its type code (e.g. `T`/`t` for text/code, `D`/`d` for initialized data, `B`/`b` for BSS, `R`/`r` for read-only data), and its name. The wrapper writes discovered symbols to a result file and sets a `HasSymbols` tag to `True` or `False`.

## Supported File Types

- Object files and executables that GNU `nm` can parse via the BFD library
- Formats explicitly noted in the documentation include ELF and PE (DLL) object files

## Usage

Run on compiled object files and executables to extract their defined symbol tables, revealing function names, global/static variables, and other defined symbols (demangled where applicable). A binary that has been stripped produces no symbols and is tagged `HasSymbols: False`, which is itself a useful signal and lets downstream pipeline steps branch on whether symbol information is present. The demangled output is especially helpful when analyzing C++ binaries.

## Documentation

[GNU Binutils nm documentation](https://sourceware.org/binutils/docs/binutils/nm.html)

## License

GPL-3.0-or-later — part of GNU Binutils; see the [GNU GPL v3](https://www.gnu.org/licenses/gpl-3.0.html).
