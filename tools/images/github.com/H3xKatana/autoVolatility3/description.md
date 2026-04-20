# autoVolatility3

autoVolatility3 is a Python script that automates memory forensics analysis with Volatility 3, running a configurable suite of plugins against a memory dump file in a single pass.

## Overview

Instead of invoking each Volatility 3 plugin manually, autoVolatility3 executes a predefined set of plugins automatically and saves the output of each plugin to its own file. It offers minimal, normal, and full scan profiles that progressively add coverage (system info and process listing at the minimal level; modules, network connections, and malware detection at the normal level; file/socket scanning, registry analysis, and scheduled tasks at the full level). In this Thorium deployment the script always runs the full scan, and the image bundles the official Volatility 3 symbol packs for Windows, Linux, and macOS.

## Supported File Types

- Memory dump files (e.g. raw `.dmp` memory images) consumable by Volatility 3

## Usage

Run autoVolatility3 on captured memory images when you need rapid triage during incident response or memory forensics. A single run produces process lists and trees, command lines, loaded modules and DLLs, network/socket connections, registry data, scheduled tasks, security identifiers, and malware-detection output, making it a good first pass to surface artifacts before deeper manual Volatility 3 analysis.

## Documentation

[H3xKatana/autoVolatility3 on GitHub](https://github.com/H3xKatana/autoVolatility3)

## License

The authoritative documentation does not state a license.
