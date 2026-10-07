---
name: Data architecture
description: Data models and storage decisions that stay correct, isolated and evolvable.
---

- Name the entities, their keys and relationships, and which data belongs to which tenant/user.
- Say where each piece of data lives (relational DB, time-series, cache, files) and why.
- Point out integrity rules the specification must state: uniqueness, required fields,
  cascade or restrict on delete.
- Plan schema evolution: how existing data migrates, what must stay backward compatible.
- Flag isolation risks: queries that could return another tenant's or user's rows.
