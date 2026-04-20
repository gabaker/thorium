# sqlitediff

Computes the content differences between two SQLite database files, emitting the SQL statements needed to transform one into the other.

## Images Run

- **sqlitediff** — Runs the official SQLite `sqldiff` utility to compare two SQLite databases, producing a forward diff (A to B), a reverse diff (B to A), and a textual summary of the differences as INSERT, UPDATE, DELETE, and schema-change SQL statements.

## Supported File Types

- SQLite database files (two databases are compared)

## Usage

Run this pipeline to identify how one SQLite database differs from another, such as when comparing snapshots of the same database taken at different points in time. It is useful for forensic analysis of SQLite databases that commonly appear in browser profiles, mobile device backups, and application data, and for detecting modifications or tampering in application databases.
