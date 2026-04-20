# Trivy

Runs Aqua Security's Trivy scanner in filesystem mode against a submitted sample to surface known vulnerabilities, misconfigurations, embedded secrets, and software license information.

## Images Run

- **trivy** — Invokes `trivy fs --scanners vuln,secret,misconfig,license` on the submitted file or directory, detecting OS packages and software dependencies (SBOM), known vulnerabilities (CVEs), Infrastructure-as-Code misconfigurations, sensitive information/secrets, and software licenses, emitting both a human-readable table and a JSON report.

## Supported File Types

This pipeline treats the submitted file or directory tree as a filesystem target. Within that target, Trivy analyzes language dependency/lock files and software dependencies (for SBOM generation), OS package metadata, Infrastructure-as-Code and configuration files (for misconfiguration checks), arbitrary files for embedded secrets and sensitive information, and files for software license identification.

## Usage

Run this pipeline on filesystem artifacts, application packages, project directories, or extracted container images to identify known vulnerabilities (CVEs) in dependencies, configuration and IaC misconfigurations, embedded secrets, and software license details. It is well suited to software supply chain security and to spotting vulnerable or risky components within compiled artifacts, application bundles, or source trees.
