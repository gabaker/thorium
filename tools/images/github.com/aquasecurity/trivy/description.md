# Trivy

Trivy is a comprehensive and versatile security scanner that looks for security issues across a range of targets. In this Thorium image it runs in filesystem (`fs`) mode against the submitted file or directory.

## Overview

Trivy combines multiple scanners over multiple targets (container images, filesystems, remote git repositories, virtual machine images, and Kubernetes). Its scanners detect OS packages and software dependencies in use (SBOM), known vulnerabilities (CVEs), Infrastructure-as-Code issues and misconfigurations, sensitive information and secrets, and software licenses. It supports most popular programming languages, operating systems, and platforms. As deployed here, the wrapper invokes `trivy fs --scanners vuln,secret,misconfig,license --include-dev-deps` on the input, emitting both a human-readable table and a JSON report.

## Supported File Types

This image scans the submitted file or directory tree as a filesystem target. Within that target, Trivy can analyze:

- Language dependency/lock files and software dependencies (SBOM generation)
- OS package metadata
- Infrastructure-as-Code and configuration files (misconfiguration checks)
- Arbitrary files for embedded secrets and sensitive information
- Files for software license identification

## Usage

Run Trivy on filesystem artifacts, application packages, project directories, or extracted images to surface known vulnerabilities (CVEs) in dependencies, configuration and IaC misconfigurations, embedded secrets, and software license information. It is well suited to software supply chain security and to identifying vulnerable or risky components within compiled artifacts, application bundles, or source trees.

## Documentation

[Trivy Documentation](https://trivy.dev/docs/latest/)

## License

Apache-2.0
