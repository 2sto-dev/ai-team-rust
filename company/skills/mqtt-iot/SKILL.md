---
name: MQTT and IoT
description: MQTT topic design, device messages and telemetry handling that fail safe.
---

- Topics are hierarchical and scoped by tenant and device, e.g.
  `tenants/{tenant}/devices/{device}/up/{stream}` and `.../down/{kind}`; no wildcards in publishes.
- Never trust a topic's claims: check that the device belongs to the tenant in the topic and drop
  (and log) messages that do not match.
- Payloads are versioned JSON (or a documented binary format); unknown fields are ignored, missing
  required fields reject the message.
- Choose QoS deliberately (telemetry usually 0/1, commands 1) and make command handling
  idempotent.
- Devices authenticate individually; credentials and ACLs are per device, never shared.
