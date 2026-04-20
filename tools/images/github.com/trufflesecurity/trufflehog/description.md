# TruffleHog

TruffleHog is a secrets discovery, classification, validation, and analysis tool that finds leaked credentials such as API keys, database passwords, and private encryption keys. In this Thorium deployment it scans a git repository (via `trufflehog git`) and emits JSON results.

## Overview

TruffleHog classifies over 800 secret types and maps each finding back to the specific identity it belongs to (e.g. AWS, Stripe, Postgres, SSL private keys). For every secret it can classify, it can actively verify the credential against the relevant service's API to confirm whether it is live, and for the most commonly leaked credential types it can perform deeper analysis of the resources and permissions a secret grants. Here it is invoked against the supplied repository path (`git file://<path>`), walking commit history to surface secrets introduced anywhere in the repo.

## Supported File Types

- Git repositories (commit history, files, and metadata)

This deployment runs the `git` subcommand against a repository path. While TruffleHog upstream also supports filesystems, archives, binaries, documents, and many remote sources, this image specifically scans git repos.

## Usage

Run TruffleHog when you need to detect hardcoded or accidentally committed credentials within a git repository, including secrets that were added (and possibly later removed) across its commit history. It is especially useful for surfacing exposed API keys, passwords, tokens, and private keys, and its active verification distinguishes live credentials that pose an immediate risk from inactive or unverifiable ones. Use it as part of repository triage or supply-chain analysis where leaked secrets are a concern.

## Documentation

[TruffleHog on GitHub](https://github.com/trufflesecurity/trufflehog)

## License

GNU Affero General Public License v3.0 (AGPL-3.0), as stated in the project documentation for TruffleHog v3.
