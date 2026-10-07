---
name: Django and DRF
description: Django apps with Django REST Framework APIs, migrations and tests.
---

- One concern per app; models, serializers, views and urls in their usual modules.
- Every model change comes with its migration; never edit an applied migration.
- APIs use DRF serializers and viewsets/generic views with explicit permission classes;
  querysets are filtered by the requesting user's scope (tenant, owner) - never return all rows.
- Validate input in serializers; return proper status codes (400, 403, 404), not 200 with errors.
- Tests use the project's runner (`pytest` with pytest-django, or `manage.py test`) and cover
  permissions, not just the happy path.
