---
name: Secure code review
description: What a security reviewer must reject, and how to report it.
---

Reject the implementation (`CHANGES_REQUIRED`) when you find any of these:

- secrets, tokens or passwords in code, tests, logs or error messages;
- untrusted input reaching `eval`, `exec`, shell commands, SQL or file paths without
  validation (path traversal with `..`, absolute paths);
- unsafe deserialization (`pickle` of untrusted data, `yaml.load` without a safe loader);
- disabled TLS verification, hard-coded credentials, overly broad file permissions;
- error handling that swallows failures silently where security depends on them.

Do not reject for style, naming or missing features that are not security issues; the
Reviewer judges those. Approve (`APPROVED`) when none of the above applies.

Feedback names the file, the problem and the concrete fix, one item per problem.
