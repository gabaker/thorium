# SQLite Dump

Inspects a SQLite database file with the `sqlite3` command-line shell to recover its internal header metadata, schema, and table names without parsing the binary format by hand.

## Images Run

- **sqlitedump** — Runs the `sqlite3` shell against a SQLite database, reporting the database header/internal metadata (`.dbinfo`) and table list (`.tables`) as JSON results and dumping the schema CREATE statements (`.schema`) to a `schema.sql` result file.

## Supported File Types

- SQLite database files

## Usage

Run this pipeline on SQLite database files to recover their structure and internal metadata. SQLite databases are common in application forensics, backing web browser history, cookies, and bookmarks as well as mobile apps and messaging platforms. Inspecting the header info, schema, and table list helps identify what data a database holds and guides further extraction.
