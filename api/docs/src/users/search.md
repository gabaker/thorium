# Search

Thorium also allows users to search through tool results and file tags to find
interesting files. This is currently only available in the Web UI and can be accessed on
the home page. Thorium uses the [Lucene syntax](https://www.elastic.co/guide/en/kibana/current/lucene-query.html)
for search queries.

It's important to note that documents are indexed **per group**. This means that
for a document to be returned, **all** search parametes must be met by at least
one group (see the below [FAQ](#what-do-groups-have-to-do-with-searching-in-thorium)
for more details).

<p align="center">
    <img width="800" src="./../static_resources/search.png">
</p>

### Indexes

Data is stored in Elasticsearch in various indexes based on the data type (i.e. results are
stored separately from tags). You can select the index to search on by selecting the dropdown
on the right of the search bar. Searches are performed on all indexes by default.

<p align="center">
    <img width="800" src="./../static_resources/search-dropdown.png">
</p>

### Search Parameters

You can specify various parameters for your search by clicking the funnel icon below the search bar.
Parameters include the sample's group(s), the date the sample was last modified, and the number of search
results to display per page. By default, no groups are selected and searches are performed on all
of the user's groups.

<p align="center">
    <img width="800" src="./../static_resources/search-parameters.png">
</p>

### Examples

The following are examples of possibl search queries using Lucene syntax.

---

Querying for tags/results containing the text `pe32`:

```
pe32
```

Querying for tags/results containing `pe32` or `Microsoft`:

```
pe32 OR Microsoft
```

Querying for tags/results containing `rust` and `x86_64`:

```
rust AND x86_64
```

Querying for tags/results containing the string `rust and x86_64`. Use quotes to
wrap search queries that contain white space or conditional keywords:

```
"rust and x86_64"
```

Querying for tags/results containing the string `rust and x86_64` and `pe32`:

```
"rust and x86_64" AND pe32
```

Querying for tags/results containing `pe32` or string `rust and x86_64` and `pe32`:

```
pe32 OR ("rust and x86_64" AND pe32)
```

All UTF-8 text is supported:

```
✨Some 🤪 UTF-8 😁 text!!!✨
```


#### Results

Querying for results where a field named `PEType` is set to `"PE32+"`

```
"PEType:\"PE32+\""
```

#### Tags

> Note: Tags are stored in Elasticsearch with `=` delimiting the key and value.
> That means that the most effective way for searching for whole tags is by
> using `=`. Using other delimiters (e.g. `:`) may cause otherwise
> matching tags to not appear.

Querying for the tag `arch=x86_64`:

```
arch=x86_64
```

Querying for tags *exactly* matching `arch=x86_64` (use quotes):

```
"arch=x86_64"
```

Querying for files with either the tag `arch=x86_64` or `arch=arm64`:

```
arch=x86_64 OR arch=x86_64
```

Querying for files with both tags `Lang=Rust` and `arch=x86_64`:

```
Lang=Rust AND arch=x86_64
```

If tags contain Lucene keywords (e.g. `AND`, `OR`, etc.), use quotes:

```
"Tag With Spaces=Keywords AND OR" AND "Tag=Value"
```


## Grep Search

Elasticsearch based search only covers data that the `search-streamer` has indexed, which
means it cannot see the *contents* of result files at all. Grep search fills that gap by
reading data straight out of Scylla/S3 and running a regex over it with the same engine
[ripgrep](https://github.com/BurntSushi/ripgrep) uses. It is slower than an Elasticsearch
query, but it can search anything Thorium stores.

Grep searches are always sent as `POST` requests. The scoping and paging options ride in
the query string using the same forms as the list routes (`limit=10`, `groups[]=plants`),
while the regex itself is sent in a JSON body since regexes do not survive being crammed
into a query string.

### Searching a single item

To grep a single sample, repo, or entity, `POST` your pattern to that item's key:

```
POST /api/files/grep/<SHA256>?case_insensitive=true
POST /api/repos/grep/<REPO_URL>?case_insensitive=true
POST /api/entities/grep/<ENTITY_ID>?case_insensitive=true
```

```JSON
{
    "pattern": "corn[0-9]+"
}
```

This returns the matches for just that item:

```JSON
{
    "key": "<SHA256>",
    "results": {
        "CornHarvester": {
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8": {
                "result": {
                    "matches": [
                        {"line": 12, "offset": 431, "len": 6, "snippet": "corn42 was found"}
                    ],
                    "searched": 8192,
                    "truncated": null
                }
            }
        }
    },
    "result_files": {},
    "submissions": {},
    "comments": {},
    "tags": {},
    "metadata": {}
}
```

Matches are nested by the source they came from: `results` and `result_files` are keyed by
tool, then result id, then field/file name; `submissions` is keyed by submission id and then
field; `comments` is keyed by comment id; `tags` is keyed by tag key and then tag value; and
`metadata` is keyed by field (currently entities only).

Each document also reports how many bytes were `searched` and why it was `truncated` if it
was only partially searched. `truncated` is `null` when a document was fully searched and
one of the following reasons otherwise:

- `"Binary"` - binary data (a NUL byte) was detected part way through this document
- `"MaxDocumentSize"` - this document was larger than the cluster's configured max document size
- `"MatchBudget"` - the query wide match limit was reached while searching this document; on
  corpus searches the rest of this document's matches are returned by continuing the cursor,
  while on single item searches raising `max_matches` or narrowing `sources` is the only way
  to see more of them
- `"Error"` - this document could not be fully searched due to an error (a failed download or
  a line larger than the configured max line size)

Documents that were truncated or errored without any matches are omitted from responses by
default so unsearchable documents never bury your actual hits; pass `with_truncated=true` to
see them. Documents that found matches before being cut short are always returned with their
`truncated` reason. Binary documents where nothing was searched at all are omitted entirely
either way since they can never match a text query.

### Searching the whole corpus

To grep everything you can see, `POST` your pattern to the grep route without a key:

```
POST /api/files/grep/?limit=10&groups[]=plants
POST /api/repos/grep/?limit=10&groups[]=plants
POST /api/entities/grep/?limit=10&groups[]=plants
```

```JSON
{
    "pattern": "corn[0-9]+"
}
```

This returns a cursor instead of a single item's matches:

```JSON
{
    "cursor": "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
    "data": [
        {"key": "<SHA256>", "results": {}, "...": "..."}
    ],
    "metadata": {"found": 1, "truncated": 0, "errors": 0}
}
```

`metadata` reports what this page's scan actually did: how many documents had at least one
match (`found`), how many were only partially searched (`truncated`), and how many failed to
be read (`errors`). The counts can overlap - a document that found matches before being cut
short counts under both `found` and `truncated` - and they include the hit-less truncated
documents that are omitted from `data` by default, so an empty page with a non-zero
`truncated` count tells you the scan did real work even though nothing matched.

Because grep has to read every byte it searches, a corpus search is paged. Each page keeps
scanning items until it has `limit` items to return, its `scan_time` budget (default 50
seconds) is spent, or the corpus is exhausted; once a page has found at least one actual
match it also stops after `scan_idle` (default 3 seconds) passes without finding another,
so a partially full page still comes back promptly. Documents that were only truncated or
errored never arm that early cutoff, so a stream of unsearchable documents cannot end a
page early with no hits. A page only comes back empty when an entire scan time budget of
items had no matches at all. If a `cursor` comes back then there are still items left to
search, so send the same body again with that cursor id as a query param to pick up exactly
where the last page stopped:

```
POST /api/files/grep/?cursor=6ba7b811-9dad-11d1-80b4-00c04fd430c8
```

```JSON
{
    "pattern": "corn[0-9]+"
}
```

Keep sending the returned cursor back until the response has no `cursor` field, which means
every item has been searched. Items that were pulled off the list but not searched before the
page filled up are saved on the cursor, so no hits are skipped even when a single query has
far more hits than one page can hold.

When a page hits its `max_matches` limit partway through an item, the cursor also remembers
how far into each of that item's documents it got. Continuing the cursor finishes that item
first and only returns the matches that have not been returned yet, so paging a cursor to
exhaustion yields every match exactly once - nothing is duplicated and nothing is lost, even
when a single item has more matches than an entire page's budget. This does mean the same
item `key` can appear on more than one page, so merge pages by `key` if you are aggregating
matches per item.

The one exception is a document that fails to be read (`"truncated": "Error"`, like a failed
download): it is retried if its item is resumed again, but if its item finishes while it is
still failing then its unreturned matches are not revisited by the cursor. Re-grep that item
directly if you need them. Items deleted while a cursor is paging are simply skipped.

### Limiting what is searched

Grep reads a lot of data, so it is worth narrowing the search down to only what you care
about. Both route shapes take the same scoping options. Only `pattern` rides in the JSON
body; everything else is a query param using the same `name=value`/`name[]=value` forms as
the list routes:

| option | sent as | default | purpose |
|---|---|---|---|
| `pattern` | JSON body | required | The regex to search for |
| `case_insensitive` | query param | `false` | Ignore case when matching |
| `multi_line` | query param | `false` | Allow the pattern to match across lines (more expensive, see below) |
| `sources[]` | query param | all sources | The sources of data to search |
| `tools[]` | query param | all tools | Only search results from these tools |
| `groups[]` | query param | all your groups | Only search data in these groups (scopes every source: tags, comments, submissions, metadata, and results) |
| `start`/`end` | query param | now/the earliest item of that type | The window of items to search |
| `limit` | query param | 10 | How many items with matches to return in a single page, floored at 1 (corpus routes only) |
| `max_matches` | query param | cluster max | Stop the whole query after this many matches |
| `scan_time` | query param | cluster max (50s) | How many milliseconds a single page can scan for, capped by the cluster (corpus routes only) |
| `scan_idle` | query param | cluster max (3s) | How many milliseconds a page keeps scanning after its last new match, capped by the cluster (corpus routes only) |
| `with_truncated` | query param | `false` | Also return documents that were truncated or errored without any matches |

`sources[]` is the main lever. It accepts any of `Results`, `ResultFiles`, `Submissions`,
`Comments`, `Tags`, and `Metadata`, and defaults to all of them. Drop the ones you do not
need to skip that data entirely. For example, to search only submission info:

```
POST /api/files/grep/?sources[]=Submissions
```

To search results but skip the (potentially very large) result files in S3:

```
POST /api/files/grep/?sources[]=Results&sources[]=Tags
```

To search only the results from specific tools:

```
POST /api/files/grep/?sources[]=Results&sources[]=ResultFiles&tools[]=CornHarvester&tools[]=CornCounter
```

### Grepping with Thorctl

Thorctl wraps all of the routes above in a single ripgrep-like command that builds the
queries and pages through cursors for you:

```bash
thorctl grep '<PATTERN>' [TARGETS...]
```

With no targets `thorctl grep` walks the whole file corpus; pass `--repos` or
`--entities` to walk those corpora instead. Targets can be file SHA256s, entity UUIDs,
and repo URLs mixed freely; each target's type is detected from its shape.

```bash
# grep the file corpus for a pattern
thorctl grep 'corn[0-9]+'

# grep the repo corpus instead, scoped to one group
thorctl grep 'corn[0-9]+' --repos -g plants

# grep specific items (a sample and a repo in one command)
thorctl grep -i 'germinated' 7b2f09d1e5a64c2b9f3a8d7e6c5b4a3928170605f4e3d2c1b0a998877665c4e9 github.com/corn/stalker

# only search inline results and tags from one tool
thorctl grep 'corn[0-9]+' --sources results,tags --tools CornHarvester
```

<link rel="stylesheet" href="./../static_resources/asciinema/asciinema-player.css">
<div id="grep-demo-corpus"></div>
<script src="./../static_resources/asciinema/asciinema-player.min.js"></script>
<script>
  AsciinemaPlayer.create('./../static_resources/asciinema/grep-corpus.cast',
    document.getElementById('grep-demo-corpus'),
    { loop: true, autoPlay: true, cols: 120, rows: 28, idleTimeLimit: 2 });
</script>

On a terminal matches print grouped under their item's key, with each document's
provenance path above its `line:snippet` rows so long keys and ids only print once:

```
7b2f09d1e5a64c2b9f3a8d7e6c5b4a3928170605f4e3d2c1b0a998877665c4e9
  results/CornHarvester/6ba7b810-9dad-11d1-80b4-00c04fd430c8
    12:corn123 germinated in sector 7
    88:corn42 failed quality check
  tags/plant/corn9000
    1:corn9000

grep: 1 matched items across 3 pages · docs: 3 matched · 0 truncated · 0 errors
```

Grepping a single item works the same way but skips the cursor entirely:

<div id="grep-demo-single"></div>
<script>
  AsciinemaPlayer.create('./../static_resources/asciinema/grep-single.cast',
    document.getElementById('grep-demo-single'),
    { loop: true, autoPlay: true, cols: 120, rows: 28, idleTimeLimit: 2 });
</script>

When stdout is piped the output automatically switches to one self contained
`key:source:...:line:snippet` row per match, and `--format json` emits each item as one
JSON object per line (NDJSON) for tools like `jq`:

```bash
# collect the keys of everything that matched
thorctl grep 'corn[0-9]+' --format json | jq -r '.key'

# cut the sha256s out of flat rows
thorctl grep 'corn[0-9]+' | cut -d: -f1 | sort -u
```

<div id="grep-demo-pipe"></div>
<script>
  AsciinemaPlayer.create('./../static_resources/asciinema/grep-pipe.cast',
    document.getElementById('grep-demo-pipe'),
    { loop: true, autoPlay: true, cols: 120, rows: 28, idleTimeLimit: 2 });
</script>

Summary stats print to stderr when the search finishes so they never pollute pipes, and
a search that finds nothing exits with code 1 just like grep. A corpus search that stops
at its `--limit` prints the cursor to resume with on stderr:

```bash
thorctl grep 'corn[0-9]+' --cursor 6ba7b810-9dad-11d1-80b4-00c04fd430c8
```

Resume with the same corpus flag as the original search; the pattern and scoping options
are remembered server side so only per page knobs like `--limit` can change.

#### Thorctl grep flags

| flag | default | purpose |
|---|---|---|
| `TARGETS...` | none | Specific SHA256s, entity UUIDs, and repo URLs to grep instead of a corpus |
| `--repos` / `--entities` | files | The corpus to walk when no targets are given |
| `-i` / `--case-insensitive` | off | Ignore case when matching |
| `-U` / `--multi-line` | off | Allow the pattern to match across lines (more expensive, see below) |
| `-g` / `--groups` | all your groups | Only search data in these groups |
| `--sources` | all sources | Any of `results`, `result-files`, `submissions`, `comments`, `tags`, `metadata` |
| `--tools` | all tools | Only search results/result files from these tools |
| `-s` / `--start`, `-e` / `--end` | now / the earliest item | The window of items to search, parsed with `--date-fmt` |
| `-l` / `--limit` | 50 | Stop after at least this many matched items (checked between pages so resuming never skips matches) |
| `--no-limit` | off | Keep going until the entire corpus has been searched |
| `-p` / `--page-size` | 10 | How many items with matches to return per request (capped by the cluster) |
| `--max-matches` | cluster max | Stop the whole search after this many matches |
| `--scan-time` / `--scan-idle` | cluster max | Per request scan budgets in milliseconds |
| `--with-truncated` | off | Also list documents that were truncated or errored without any matches |
| `--cursor` | none | Resume an earlier corpus search |
| `--format` | auto | `heading` on a terminal, `flat` when piped, or `json` to force NDJSON |

> Note: Thorctl replaces control characters in snippets and provenance paths with `�`
> before printing so malicious file contents cannot inject terminal escape codes; hit the
> HTTP routes directly if you need the raw bytes (in `--format json` only snippets are
> sanitized so keys stay exact). In flat output a tag value or file name containing
> `:` can make a row ambiguous to split, so prefer `--format json` when parsing matters.

### Grep FAQ

##### Why do I have to page through a grep search?

Because grep actually reads the data. An Elasticsearch query hits an index; a grep query
downloads and scans result files. Paging keeps any single request bounded in both time
(`scan_time`, default 50 seconds) and memory, and the cursor makes sure you can still walk
the whole corpus. A page returns as soon as it fills with matches, so dense corpora page
quickly; sparse corpora scan for up to a full time budget per page instead of handing back
a stream of empty pages. Set `scan_time` lower if you would rather have fast small pages.

##### Why did my document come back `truncated`?

The `truncated` field tells you why: `"MaxDocumentSize"` means it was bigger than the
cluster's configured max document size, `"Binary"` means binary data was found in it (grep
gives up at the first NUL byte, the same as `ripgrep` does), `"MatchBudget"` means your query
hit its `max_matches` limit, and `"Error"` means something went wrong while reading or
searching it (like a failed download or a line larger than the max line size). The matches
you did get are still valid, they just come from the part of the document that was searched.

Documents that were truncated or errored without finding any matches are omitted from
responses by default; pass `with_truncated=true` to see them. Either way the corpus response
`metadata` counts every `found`, `truncated`, and `errors` document the page scanned, and
the flag is per request, so you can flip it between pages of the same cursor. Binary
documents where nothing was searched at all (like images) are never reported; they are
omitted entirely since they can never match.

##### Does `max_matches` make me lose matches?

Not on corpus searches. The cursor remembers how far into each document the budget got, so
continuing it returns the matches that were cut off without ever repeating one; just keep
paging until no `cursor` comes back. On the single item routes there is no cursor, so
`max_matches` is a hard cap there - raise it or narrow `sources` to see more.

##### Why is `multi_line` more expensive?

Single line searches stream through result files, holding only one line at a time in memory
no matter how large the file is. A pattern that can match across lines has to buffer the
whole document, so `multi_line` searches are bounded by the max document size and are more
likely to come back truncated with `"MaxDocumentSize"`.

##### Why was my pattern rejected?

Grep compiles your pattern with size limits on both the regex and its DFA so a pathological
pattern cannot eat the API's memory. A pattern that is invalid, or that is too large to
compile inside those limits, returns a `400` rather than failing the search part way through.


### FAQ

##### Why does it take some time for data to become searchable?

It can take some time (usually < 10 seconds) for results to be searchable in
Thorium because they are indexed asynchronusly by a separate component called
the Thorium `search-streamer`. The time it takes for the `search-streamer`
to index data depends on how much data has been added/modified/deleted in
Thorium recently.

##### What do groups have to do with searching in Thorium?

Due to Thorium's permissioning requirements and how Elastic operates, a file will
have a separate document in Elastic for each group containing results/tags visible
in that group. That means that for a file to appear in the search results, **all** of
the given search parameters must for at least one group.


For example, let's say we have one sample with a placeholder SHA256 of `my-sha256`. This
sample is in two groups, `GroupA` and `GroupB`. The file has tags in both groups, but
not all of them are shared between groups. Our data would look something like this:

```JSON
[
    {
        "sha256": "my-sha256",
        "group": "GroupA",
        "tags": [
            "Corn": "IsGood",
            "HasTaste": "true",
            "Fliffy": "IsAGoodDog"
        ]
    },
    {
        "sha256": "my-sha256",
        "group": "GroupB",
        "tags": [
            "Corn": "IsBad",
            "HasTaste": "false",
            "Fliffy": "IsAGoodDog"
        ]
    }
]
```

So the following query would return the sample from both groups because both of the groups
have the given tag:

```
Query: "Fliffy=IsAGoodDog"

Results:

"my-sha256", "GroupA"
"my-sha256", "GroupB"
````

But this query would return no samples because *neither* of the groups have *both* of the
given tags:

```
Query: "Corn=IsBad AND HasTast=true"

Results:

```
