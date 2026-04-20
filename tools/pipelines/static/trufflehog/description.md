# TruffleHog Pipeline

Scans a git repository to discover, classify, and validate leaked credentials and secrets across its commit history, emitting JSON results.

## Images Run

- **trufflehog** — Runs TruffleHog's `git` subcommand against the supplied repository path, classifying over 800 secret types (API keys, database passwords, private encryption keys) and actively verifying whether discovered credentials are still live.

## Supported File Types

- Git repositories (commit history, files, and metadata)

This deployment invokes the `git` subcommand against a repository path. While TruffleHog upstream also supports filesystems, archives, binaries, documents, and many remote sources, this pipeline specifically scans git repositories.

## Usage

Run this pipeline when you need to detect hardcoded or accidentally committed credentials within a git repository, including secrets that were added (and possibly later removed) anywhere across its commit history. It is especially useful for surfacing exposed API keys, passwords, tokens, and private keys, with active verification distinguishing live credentials that pose an immediate risk from inactive ones. Use it as part of repository triage or supply-chain analysis where leaked secrets are a concern.
