# Commenting on Files

Commenting on files is a great way to share your progress and insights from analyzing a file. Anyone that
is a member of at least one of your groups that has access to the file will be able to view your comment.
Thorctl has no comment command yet, so comments are written and removed through the API.

## API
---

| Method | Route | Description |
|--------|-------|-------------|
| `POST` | `/api/files/comment/<SHA256>` | Add a comment (and attachments) to a file |
| `DELETE` | `/api/files/comment/<SHA256>/<COMMENT ID>` | Delete a comment |
| `GET` | `/api/files/comment/download/<SHA256>/<COMMENT ID>/<NAME>` | Download a comment's attachment |

A comment is a `multipart/form-data` request with a `comment` field holding the text and one `files` field for each
attachment. Comments are listed with the rest of a file's details (`thorctl files describe <SHA256>`).

```bash
curl -H "Authorization: Bearer $TOKEN_B64" https://<URL>/api/files/comment/<SHA256> \
  -F 'comment=Packed with UPX; unpacked copy uploaded as a child' -F files=@./notes.txt
```

A file you can't see is answered with `404` and a malformed form with `400`.

## Comment Attachments

You can also upload files as attachments to your comments. Unlike files/samples, comment attachments are not stored in the
safe CaRTed format. Only submit benign data as a comment attachment.

If you need to submit a manually modified version of
a sample, you can do so by uploading the modified sample with an Origin of `Unpacked` or `Transformed` and with the
`Parent` value set to the `SHA256` of the original file (see [Uploading Files](./uploading_files.md#api)).
