---
name: React with TypeScript
description: React function components in strict TypeScript, with data fetching and tests.
---

- Strict TypeScript: no `any`, typed props and API responses; the build (`tsc`) must pass.
- Function components and hooks only; keep components small and move logic into hooks.
- Server data through the project's data layer (e.g. TanStack Query) with query keys that include
  every parameter the data depends on (user, tenant, filters).
- Never trust the UI for authorization; hiding a button is convenience, the backend decides.
- Accessible markup: labels for inputs, buttons for actions, alt text for images.
- Tests with the project's runner (Vitest/Jest + Testing Library) when one is configured.
