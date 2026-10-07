---
name: Integration design
description: Interfaces between components: APIs, message contracts, authentication and versioning.
---

- Name every interface the task touches: HTTP endpoints, MQTT topics, queues, files, CLIs.
- For each: request/response or message shape, error cases, who may call it, how it is versioned.
- Authentication and authorization at every boundary; tokens and secrets never in URLs or logs.
- Idempotency and retries for anything that can be delivered twice.
- Backward compatibility: what existing clients or devices must keep working.
