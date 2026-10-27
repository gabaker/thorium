# Tagging Files

Tags are metadata key/value pairs that are used to label the files and repositories that Thorium ingests.
These labels make it easier for users and tools to find data that is stored in Thorium. It also helps users
understand the data at a glance. Fields like file type, compiler, and origin info can all be tagged to better
document the provenance  of your data. Tags can be added when a file or repo is initially uploaded, ingested
by the agent when a tool is run on that file, or added later by a Thorium user with `thorctl tags` or the API.

Tag values may contain any UTF-8 characters including symbols (e.g. Language=C++), characters from other languages
(e.g. CountryOfOrigin=Україна), or even emojis (e.g. Status=✅).

Tags are currently case sensitive, but tag normalization (standardizing capitalization of existing tags in a Thorium
instance) is a planned to be added in future versions of Thorium.

## Tagging on Upload (Thorctl)
---

When using Thorctl to upload one or more file(s), you can apply key/value tag pairs with the `-T key=value` or
`--file-tags key=value` flag.

```bash
$ thorctl files upload --groups demo --file-tags TagKey=sometagvalue /path/to/upload
```

If you want to specify multiple tags to apply to each file you upload, supply the `-T` or `--file-tags` flag multiple
times:

```bash
$ thorctl files upload --groups demo --file-tags Key1=value1 --file-tags Key2=value2 /path/to/upload
```


## Editing Tags (Thorctl)
---

When using Thorctl to tag an existing file, use the `tags` command.

```bash
$ thorctl tags add --add-tags platform=MacOS --files 619dddbd681b593d8c6f66de50dd41e422bfac7a83feab6c3f0944de7c73a4d3
```

To add multiple tags at once, repeat the `--add-tags` option.

```bash
$ thorctl tags add --add-tags platform=MacOS --add-tags OS_Version=13.7.4 --files 619dddbd681b593d8c6f66de50dd41e422bfac7a83feab6c3f0944de7c73a4d3
```

To delete a tag, you must specify the complete `name=value` tag to remove.

```bash
$ thorctl tags delete --delete-tags "stripped=false"  --files 619dddbd681b593d8c6f66de50dd41e422bfac7a83feab6c3f0944de7c73a4d3
```

Be aware that `thorctl` will report success for deleting a non-existent tag.

`thorctl tags get <SHA256_OR_REPO>` prints the tags on one file or repo as JSON, and `thorctl tags add` and `delete`
also take `-r/--repos` to change repo tags, `-G/--add-groups` (`--delete-groups`) to limit which of the object's groups
see the tag, and the same group, tag, and date search flags as `files get` to tag many files at once. They change at
most `--limit` objects (50 by default; `--no-limit` removes it).

## API
---

| Method | Route | Description |
|--------|-------|-------------|
| `POST` | `/api/files/tags/<SHA256>` | Add tags to a file |
| `DELETE` | `/api/files/tags/<SHA256>` | Delete tags from a file |
| `POST` | `/api/repos/tags/<URL>` | Add tags to a repo |
| `DELETE` | `/api/repos/tags/<URL>` | Delete tags from a repo |

A sample or group you can't see is answered with `404`, and a request you aren't allowed to make with `401`. Tags can also be set at upload time (see
[Uploading Files](./uploading_files.md#api)).
