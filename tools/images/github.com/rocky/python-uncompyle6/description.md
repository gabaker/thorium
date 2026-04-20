# uncompyle6

uncompyle6 is a native Python cross-version decompiler and fragment decompiler that translates Python bytecode back into equivalent Python source code. It is the successor to decompyle, uncompyle, and uncompyle2.

## Overview

uncompyle6 accepts bytecodes from Python version 1.0 through 3.8, spanning over 24 years of Python releases, and also handles Dropbox's Python 2.5 bytecode and some PyPy bytecodes. Using compiler technology, it builds a parse tree from the instructions, allowing it to classify and understand sections of Python bytecode rather than performing a naive translation. Beyond whole-module decompilation, it can deparse just fragments of source code and report source-code information around a given bytecode offset, which is useful for showing stack-trace locations at runtime when source is unavailable. Decompilation quality is strongest for Python 2 and for Python 3 around versions 3.3-3.4, dropping off for versions further from that range.

## Supported File Types

- Compiled Python bytecode files (`.pyc`, `.pyo`)

In this image, the submitted sample is treated as a Python bytecode file (copied to a `.pyc` name) and decompiled with `uncompyle6`.

## Usage

Run uncompyle6 on Python `.pyc`/`.pyo` bytecode files to recover the original Python source code. This is valuable when analyzing Python-based malware, inspecting bytecode extracted from packaged or embedded Python applications, or recovering source from compiled Python where the original `.py` files are unavailable. Note that it does not handle obfuscated or encrypted bytecode (e.g., PJOrion, most Dropbox interpreters) or non-CPython bytecode such as Cython or MicroPython, and some constructs—particularly complex control flow—may decompile imperfectly.

## Documentation

[python-uncompyle6 on GitHub](https://github.com/rocky/python-uncompyle6)

## License

GPL-3.0 — see the project's [COPYING](https://github.com/rocky/python-uncompyle6/blob/master/COPYING).
