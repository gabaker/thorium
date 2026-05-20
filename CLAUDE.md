# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Thorium is a scalable file analysis and data generation platform that orchestrates arbitrary Docker/VM/shell tools at scale. It runs on Kubernetes and is built by CISA/Sandia National Laboratories. The backend is a Rust workspace; the frontend is a React/TypeScript SPA.

## Build Commands

### Rust (requires nightly toolchain)

```bash
# Build all workspace crates
cargo build --release --features vendored-openssl

# Build a single crate
cargo build -p thorium-api
cargo build -p thorctl

# Generate developer docs
cargo doc --no-deps

# Build user-facing mdbook docs
mdbook build api/docs
```

### Frontend (ui/)

```bash
cd ui
npm install
npm run build              # production build (tsc + vite)
npm run build-check        # full type-check build (includes .jsx checkJs)
npm run build-preview      # preview mode build
npm run dev                # vite dev server on port 8000
npm run lint               # eslint
npm run fix                # eslint --fix
npm run validate-style     # prettier check
npm run format             # prettier write
npm run preview            # vite preview server
npm run test               # vitest (unit tests, single run)
npm run test:watch         # vitest (watch mode)
npm run test:e2e           # playwright E2E tests (headless)
npm run test:e2e:headed    # playwright E2E tests (visible browser)
```

E2E tests require `THORIUM_API_URL=http://localhost:8080` and a running API (see [E2E Testing Guide](ui/e2e/TESTING.md)).

### Cross-compilation

Binaries are cross-compiled for Linux (musl), macOS (x86_64 + aarch64), and Windows (x86_64). The CI uses `houseabsolute/actions-rust-cross` with `--features vendored-openssl`. RUSTFLAGS typically includes `-C target-feature=+aes,+sse2` on x86_64 targets.

## Testing

### Rust Integration Tests

Integration tests live in `api/tests/` and require a running Thorium instance (ScyllaDB, Redis, Elasticsearch, S3). They use `test_utilities::admin_client()` to connect to the API configured in `api/tests/thorium-testing.yml`. Config values can be overridden with environment variables (e.g. `THORIUM__THORIUM__S3__ENDPOINT`).

```bash
# Run all integration tests
cargo test -p thorium-api --features test-utilities

# Run a single test
cargo test -p thorium-api --features test-utilities -- identify

# Run sync client tests
cargo test -p thorium-api --features test-utilities,sync -- health_blocking
```

### Frontend Unit Tests (Vitest)

Unit tests live alongside source in `ui/src/` using the `*.test.ts` pattern. Vitest is configured in `ui/vitest.config.ts` with path aliases matching `vite.config.ts`.

```bash
cd ui
npm run test          # single run
npm run test:watch    # watch mode
```

### Frontend E2E Tests (Playwright)

Playwright tests live in `ui/e2e/` and run against a live Thorium instance. They seed test data via the API, drive a headless Chromium browser, and capture screenshots for visual validation.

See the full guide: **[ui/e2e/TESTING.md](ui/e2e/TESTING.md)**

```bash
cd ui
THORIUM_API_URL=http://localhost:8080 npm run test:e2e
```

## Dev Environment (minithor)

`minithor/` contains a unified script (`minithor/minithor`) for running a single-node Thorium cluster on minikube. All commands are subcommands of `minithor/minithor`.

| Command | Purpose |
|---------|---------|
| `minithor/minithor minikube install` | Install minikube, start the k8s cluster, and enable addons (first-time only) |
| `minithor/minithor deploy` | Deploy all Thorium services (creates test user `test`/`INSECURE_DEV_PASSWORD`) |
| `minithor/minithor start` | Restart the cluster after a stop or reboot |
| `minithor/minithor expose` | Port-forward the API to `localhost:8080` |
| `minithor/minithor expose --dev` | Also forward Elastic, Kibana, Redis, MinIO, Scylla |
| `minithor/minithor expose --status` | Check which port-forwards are running |
| `minithor/minithor expose --stop` | Stop all port-forwards |
| `minithor/minithor get-config` | Extract `thorium.yml` from cluster secret to `~/thorium.yml` |
| `minithor/minithor stop` | Stop minikube |
| `minithor/minithor minikube delete --confirm` | Delete the minikube cluster |
| `minithor/minithor cleanup --confirm` | Remove all Thorium resources for a fresh deploy |

**First-time setup:**
```bash
minithor/minithor minikube install  # install minikube and start the k8s cluster
minithor/minithor deploy            # deploy Thorium services (creates test user test/INSECURE_DEV_PASSWORD)
minithor/minithor expose            # port-forward API to localhost:8080
cd ui && npm run dev                # start frontend dev server on port 8000
```

**After a reboot or `minithor stop`:**
```bash
minithor/minithor start             # restart the cluster
minithor/minithor expose            # re-establish port-forwards
cd ui && npm run dev
```

**Minikube kubeconfig note:** When using the docker driver, `minikube update-context` sets the server to the container's internal IP which is unreachable from the host. The `minithor/start` script fixes this by running `docker port minikube 8443/tcp` to get the correct localhost port mapping.

## Architecture

### Workspace Crates

The Rust workspace (`Cargo.toml`) contains these binaries and libraries:

- **api** (`thorium-api`) — The core REST API server. Uses Axum. Exposes the `thorium` library crate containing the API server, client, and data models.
  - `api/src/routes/` — HTTP route handlers (files, images, pipelines, jobs, search, MCP, etc.)
  - `api/src/models/` — Data models with backend implementations (`models/backends/` for DB layer)
  - `api/src/client/` — Async Rust client for the API (also exposes a blocking/sync client and Python bindings via feature flags)
  - `api/src/conf.rs` — Configuration loading from YAML (`thorium-template.yml`)
  - `api/src/test_utilities/` — Integration test harness (behind `test-utilities` feature)
- **operator** (`thorium-operator`) — Kubernetes operator that manages Thorium cluster lifecycle.
- **scaler** (`thorium-scaler`) — Schedules analysis jobs across available compute.
- **agent** (`thorium-agent`) — Runs on worker nodes, claims and executes analysis jobs.
- **reactor** (`thorium-reactor`) — Spawns containers directly on Linux/Windows nodes.
- **thorctl** — CLI tool for users (file upload, image/pipeline management, search, toolbox import/export).
- **thoradm** — Admin CLI for backup, settings management, provisioning.
- **event-handler** (`thorium-event-handler`) — Processes system events asynchronously.
- **search-streamer** (`thorium-search-streamer`) — Streams data into Elasticsearch for search.
- **thorium-derive** — Proc-macro crate for deriving Scylla serialization and Python bindings.
- **cart-rs** — Streaming cart/uncart library for Thorium's custom archive format.

### Python Client (workspace members)

`python/thorpy/` — PyO3-based Python bindings for the Thorium client. `python/thorpy-stubs/` provides type stubs. Both are Rust workspace members.

### Frontend (ui/)

React 19 + TypeScript SPA built with Vite.

```
ui/
  src/
    Thorium.tsx         # app root / router
    main.tsx            # entry point
    assets/             # static assets (banner text, etc.)
    components/
      associations/     # graph visualizations
        graph/          # 3D force graph (three.js/WebGL)
          controls/     # toolbar with Graph, Forces, Nodes, Edges, Export sections
          data.ts       # pure data conversion functions (testable without DOM)
          AssociationGraph.tsx
        browsing/       # tree views
        data/           # GraphDataContext (shared data provider)
        shared/         # EdgeInfo, NodeInfo, scaling utilities
      entities/         # entity system (config-driven browsing, create, details)
        browsing/       # EntityBrowsing + per-type configs
        create/         # EntityCreate + per-type configs
        details/        # EntityDetails + per-type configs + override_pages
        shared/         # shared entity components
      pages/            # layout components (NavBanner, file/image sub-panels)
      shared/           # reusable components (alerts, badges, forms, inputs)
        inputs/         # form inputs (code/CodeEditor, selectable, tags/TagSelect)
      tags/             # tag display/edit components
      tools/            # analysis tool components
    dashboards/         # dashboard views (IncidentSummary)
    fonts/
    models/             # TypeScript type definitions (associations, files, users, etc.)
    pages/              # route-level page components
      GraphBuilder.tsx  # graph page (/graph) — renders seed input + graph + tree
      Home.tsx, Login.tsx, Pipelines.tsx, NotFound.tsx
      entities/         # entity CRUD pages (collections, devices, vendors, file_systems)
      files/            # file browsing, details, upload
      images/           # image browsing, create
      reactions/        # reaction status, stage logs
      repos/            # repo browsing, details
      system/           # system settings, stats
      test/             # dev-only test pages
        code/           # editor test pages (sigma, yara, image/pipeline)
      users/            # user browsing, profile, groups
    styles/             # SCSS global styles + TS spacing/scaling utilities
    thorpi/             # API client layer (axios-based, one module per resource)
      client.ts         # axios instance with auth interceptor
      associations.ts, entities.ts, files.ts, images.ts, pipelines.ts, ...
    utilities/          # shared utilities
      auth.tsx          # AuthContext, RequireAuth, login/logout hooks
      rules/            # rule checkers (sigma, yara, image)
      transforms/       # data transforms (image, pipeline)
  e2e/                  # Playwright E2E tests
    helpers.ts          # shared test utilities (API client, auth, screenshots)
    *.spec.ts           # test specs (graph, upload, sidebar, images, pipelines, etc.)
    screenshots/        # screenshot output (gitignored)
    TESTING.md          # full testing guide for agents
  playwright.config.ts
  vitest.config.ts
  vite.config.ts
```

**Path aliases** (configured in `vite.config.ts` and `vitest.config.ts`; most are also in `tsconfig.base.json` for IDE support):

| Alias | Path |
|-------|------|
| `@assets` | `src/assets` |
| `@components` | `src/components` |
| `@entities` | `src/components/entities` |
| `@models` | `src/models` |
| `@pages` | `src/pages` |
| `@styles` | `src/styles` |
| `@thorpi` | `src/thorpi` |
| `@utilities` | `src/utilities` |

**TypeScript configs:**
- `tsconfig.base.json` — shared compiler options and path aliases
- `tsconfig.json` — extends base, includes `checkJs: true` (catches errors in .jsx files)
- `tsconfig.prod.json` — extends base, `noEmit: true`, used by `npm run build` (no checkJs)

### Backing Services

Thorium depends on: ScyllaDB (primary database), Redis (caching/queues), Elasticsearch (search indexing), S3-compatible storage (file/result/repo storage).

### Key Feature Flags (api crate)

Default features: `api`, `client`, `trace`, `ai`.

- `api` — Server-side dependencies (Axum, ScyllaDB, Redis, S3, etc.)
- `client` — Async client for the API
- `sync` — Blocking/synchronous client wrapper
- `ai` — OpenAI and MCP client integrations
- `trace` — Distributed tracing support
- `python` — PyO3 Python bindings
- `k8s` — Kubernetes error types
- `test-utilities` — Integration test infrastructure
- `vendored-openssl` — Static OpenSSL linking for cross-compilation
- `search-streamer` — Search streamer-specific code
- `scylla-utils` — ScyllaDB utility functions and derive macros

### Deployment

- **k8s/** — Kustomize-based Kubernetes manifests (bases, patches, secrets)
- **minithor/** — Single-node deployment scripts (minikube-based, dev/testing only)
- **megathor/** — Full cluster deployment (Ansible-based with roles, inventory, playbooks)
- **tools/** — Analysis tool images and pipelines. `toolbox.json` defines available tools. `tools/images/` contains Dockerfiles organized by source organization.

### Configuration

The API is configured via YAML files (see `api/thorium-template.yml` for the schema). Config sections: `thorium` (core settings, S3 buckets, retention, tracing), `redis`, `scylla`, `elastic`. The `config` crate is used with environment variable overrides using double-underscore separators (e.g. `THORIUM__SCYLLA__NODES`).

Config files matching `**/thorium*.yml` are gitignored (except templates and test configs) to prevent credential leaks.

### Agent Skills

The `skills/` directory contains Claude Code agent skills for Thorium workflows:

- **[skills/THORIUM.md](skills/THORIUM.md)** — Base skill for AI agents interacting with a Thorium instance (discovery, API auth, routes, thorctl, thoradm, minithor)
- **skills/developer/** — Development workflow skills (UI, backend, testing)
- **skills/user/** — User-facing workflow skills (file analysis, search, reactions)
- **skills/admin/** — Admin workflow skills (backup, provisioning, system management)

### Additional Project Files

- **base/** — Base Docker images for CI builds
- **Dockerfile** — Root project Dockerfile
- **.github/workflows/** — CI/CD pipeline definitions
- **ui/bundle/** — Bundle chunking configuration (chunks.json)
- **ui/mitre_tags/** — MITRE ATT&CK and MBC tag data (git submodule)
