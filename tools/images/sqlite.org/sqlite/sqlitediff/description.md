# sqldiff

sqldiff is a utility program from SQLite that computes the differences in content between two SQLite database files.

## Overview

sqldiff compares two SQLite databases and emits the SQL statements (such as INSERT, UPDATE, DELETE, and schema changes) needed to transform the first database into the second. In this deployment it produces both a forward diff (database A to B) and a reverse diff (database B to A), along with a textual summary of the differences. The tool is part of the official SQLite distribution.

## Supported File Types

- SQLite database files (two databases are compared)

## Usage

Run sqldiff to identify how one SQLite database differs from another, for example when comparing snapshots of the same database taken at different points in time. This is useful for forensic analysis of SQLite databases that commonly appear in browser profiles, mobile device backups, and application data, and for detecting modifications or tampering in application databases.

## Documentation

[SQLite sqldiff documentation](https://www.sqlite.org/sqldiff.html)

## License

Public Domain. The SQLite source code (including sqldiff) is released into the public domain; its author disclaims copyright.
