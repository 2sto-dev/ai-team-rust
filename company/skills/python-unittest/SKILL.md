---
name: Python with unittest
description: Small, standard-library Python modules with unittest tests that the platform runs.
---

- Use only the Python 3 standard library unless the project rules allow more.
- Put each package's code in its own folder with an `__init__.py`; keep modules small.
- Tests live in `tests/` as `test_*.py`, use `unittest.TestCase`, and import the code by its
  package name (the platform runs tests from the workspace root).
- Every public function gets at least one normal case and one edge case (empty input, unusual
  characters, boundaries). Assert exact values, not just types.
- Never print in library code; raise `ValueError`/`TypeError` with a clear message instead.
- When unsure how a standard-library function behaves, check its documentation with your
  `pydoc` tools (e.g. `pydoc__lookup` with `unicodedata.normalize`) instead of guessing.
