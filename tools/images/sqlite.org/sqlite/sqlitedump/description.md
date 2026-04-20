# SQLite Dump

SQLite Dump uses the `sqlite3` command-line shell to extract metadata from a SQLite database file, reporting its header/database information, schema, and table names.

## Overview

This tool runs the `sqlite3` shell against a SQLite database file and collects three pieces of information. It uses the `.dbinfo` command to read the database header/internal metadata (such as page size, encoding, and version fields), formatting the output as a JSON results object. It uses `.schema` to dump the database's CREATE statements to a `schema.sql` result file, and `.tables` to enumerate the non-system table names, which are added to the JSON results.

## Supported File Types

- SQLite database files

## Usage

Run SQLite Dump on SQLite database files to recover their structure and internal metadata without parsing the binary format by hand. SQLite databases are common in application forensics — they back web browser history, cookies, and bookmarks, as well as mobile apps and messaging platforms — so inspecting the header info, schema, and table list helps identify what data a database holds and guides further extraction.

## Documentation

[SQLite Command-Line Shell](https://www.sqlite.org/cli.html)

## License

Public Domain (SQLite is in the public domain).
