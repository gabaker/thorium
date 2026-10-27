# Architecture
---
Since Thorium is a distributed system, it has many components that work together
to facilitate file analysis. The core components are:

- API
- Scaler
- Agent
- Reactor
- Event Handler
- Search Streamer
- Operator (on Kubernetes)
- Tracing/Logging

Tools, AI agents, workflows, and model endpoints are scheduled and run by these same components; see
[Tools, Runs, and Models](../admins/agentic/agentic.md) for how they fit together.
