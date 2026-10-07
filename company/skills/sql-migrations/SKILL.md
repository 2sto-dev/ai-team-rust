---
name: SQL and migrations
description: Schemas, queries and reversible migrations that keep existing data safe.
---

- Every schema change is a migration; migrations are reversible when the tool allows it.
- Never lose data: add columns as nullable or with a default, backfill, then tighten.
- Parameterized queries only; never build SQL by string concatenation with input.
- Index the columns used in joins and frequent filters; say why in the migration.
- Use transactions for multi-step changes; keep long locks out of hot tables.
- Write tests or check scripts that run the migration on an empty database.
