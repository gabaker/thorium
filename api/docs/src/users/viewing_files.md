# Viewing Files

Once you have uploaded files to Thorium, you will likely want to find them again and read their tags and tool results.
Thorctl lists, counts, and describes files; the same data is available from the API.

## Thorctl
---

List the files in your groups with:

```bash
thorctl files get
```

The default maximum limit of files to be displayed is 50. You can change the limit by specifying the `-l/--limit` flag,
or remove it with `--no-limit` (careful: that can be millions of files):

```bash
thorctl files get --limit 100
```

#### Filter by Group
Get a list of files belonging to a certain group or groups by adding the `-g/--groups` flag. Specify multiple groups by
separating each group with a comma:

```bash
thorctl files get --groups examples,shared
```

#### Filter by Tag
Get a list of files having certain tags by adding the `-t/--tags` flag. Unlike groups, tags must be specified each with
a separate flag, as the tags themselves may contain commas or other delimiting symbols. Tag keys and values are case
sensitive unless you add `-c/--tags-case-insensitive`:

```bash
thorctl files get --tags Dataset=Examples --tags Corn=good
```

#### Filter by Date
Limit the search to a time range with `-s/--start` and `-e/--end`. Both are UTC datetimes read with the format given by
`--date-fmt` (`%Y-%m-%dT%H:%M:%S` by default), and `--start` is the most recent time to start at, searching backwards
from there:

```bash
thorctl files get --start 2024-05-17T00:00:00 --end 2024-05-01T00:00:00
```

#### Count Files
Count files and their tags with the same filters:

```bash
thorctl files count --groups examples
```

#### Describe a File
Display all details of particular file in a JSON-like format by using the following command, supplying the file's
SHA256 hash in place of the `<SHA256>` placeholder:

```bash
thorctl files describe <SHA256>
```

`thorctl files describe` also takes the group, tag, and date filters above to describe many files at once, `-L/--list`
for a file of SHA256s, and `-o/--output` to write the details to a file.

You may notice that your files do not have many tags or results yet. Running analysis tools on files adds them; see
[Spawning Reactions](./spawning_reactions.md) and [Running Tools and Workflows](./runs.md).

## API
---

| Method | Route | Description |
|--------|-------|-------------|
| `GET` | `/api/files/` | List files (query: `groups`, `tags`, `tags_case_insensitive`, `start`, `end`, `cursor`, `limit`) |
| `GET` | `/api/files/details/` | List files with their details |
| `GET` | `/api/files/count/` | Count files |
| `GET` | `/api/files/sample/<SHA256>` | One file's details |
| `GET` | `/api/search/` | Full text search of tags and results (see [Search](./search.md)) |

Only files in groups you belong to are listed (admins see all). A SHA256 you can't see is answered with `404`.
