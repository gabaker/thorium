# API
---
The core of the Thorium software stack is a restful API. The API is responsible for
allowing all the pieces of Thorium to coordinate and accomplish tasks as a group.
The API is built so that many instances of it can run on different servers to enable
high availability (HA) and horizontal scalability. If one server that runs an API
instance fails, Thorium will continue to operate. Being horizontally scalable also
enables Thorium to support a variety of deployment sizes while tailoring resource
usage to your workload.

### Uploads/Downloads

The Thorium API streams data wherever possible when responding to user requests. This
means that when a 1 GiB file is uploaded to Thorium, it will not store the entire file
in memory at once. Instead, the API will stream it to S3 in at least 5 MiB chunks. This
drastically reduces latency and the required memory footprint of the API. The same is
also true for downloads, but instead of 5 MiB chunks, data is streamed to the client as
quickly as possible with no buffering in the API.

### REST API Reference

The API describes every REST route in OpenAPI documents, one for each area of the API (for example
pipelines, reactions, repos, search, and users), and serves them in a Swagger UI at
`/api/docs/swagger-ui/` on your Thorium instance. Pick an area from the UI's document list to see
its routes with their methods, parameters, request bodies, and the routes a run-scoped token is
refused. Each document is also served as JSON at `/api/<area>/openapi.json`, such as
`/api/pipelines/openapi.json`; the basic routes (`/api`, `/api/health`, and the like) are at
`/api/openapi.json`. An area is named for its document, not always for its route prefix: the
`stream` document describes `/api/streams/...`, the `networkpolicies` document describes
`/api/network-policies/...`, and the `reactions` document also describes the tool and workflow run
routes (`/api/tools/instances/{id}/runs` and `/api/workflows/instances/{id}/runs/...`). The two search event documents are the exceptions to the file name:
they are served at `/api/search/events/results` and `/api/search/events/tags`. The MCP endpoint
isn't a REST route and isn't in these documents (see
[The Thorium MCP Server](../admins/agentic/mcp.md)).

This user guide is served at `/api/docs/user`, and the developer documentation of the Rust code starts
at `/api/docs/dev/thorium/index.html` (rustdoc writes no page at `/api/docs/dev` itself).

### FAQS
---

### How large of a file can I upload?

This is limited to the chunk size the API is configured to use on upload.
By default, this chunk size is set to 5 MiB which allows for a max size of ~48.8 GiB.

### Why does the API buffer uploads in 5 MiB chunks?

This is the minimum chunk size required by S3 for multipart uploads.

### What databases does the API require?

A variety of databases are used to store different resources:

| Database | Use Case | Example Resources |
| -------- | -------- | -------- |
| Redis | Low latency/high consistency data | reactions and scheduling streams |
| Scylla | Higly scalable/medium latency data | file metadata, reaction logs |
| Elastic | Full text search | tool results < 1 MiB |
| S3 | Object storage | all files, tool results > 1MiB |
| Jaeger | Tracing | API request logs |
