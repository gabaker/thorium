# python-uncompyle6

Decompiles compiled Python bytecode (`.pyc`/`.pyo`) back into equivalent Python source code so analysts can read and reason about the original program logic.

## Images Run

- **python-uncompyle6** — Native Python cross-version decompiler (and fragment decompiler) that uses compiler technology to build a parse tree from bytecode instructions and recover the original Python source.

## Supported File Types

Compiled Python bytecode files (`.pyc`, `.pyo`). The submitted sample is treated as a Python bytecode file and decompiled with `uncompyle6`, which accepts bytecode from Python 1.0 through 3.8 (including Dropbox's Python 2.5 bytecode and some PyPy bytecodes).

## Usage

Run this pipeline on Python `.pyc`/`.pyo` bytecode files to recover the original Python source code. It is valuable when analyzing Python-based malware, inspecting bytecode extracted from packaged or embedded Python applications, or recovering source where the original `.py` files are unavailable. Note that it does not handle obfuscated or encrypted bytecode or non-CPython bytecode (e.g., Cython, MicroPython), and decompilation quality is strongest for Python 2 and Python 3 around versions 3.3-3.4, dropping off for versions further from that range.
