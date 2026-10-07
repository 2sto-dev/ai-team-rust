---
name: Go with modules
description: Idiomatic Go packages with table-driven tests run by go test.
---

- One module (`go.mod`); packages are small and named by what they provide.
- Errors are returned and wrapped with context (`fmt.Errorf("...: %w", err)`); no panics in
  library code.
- Tests in `*_test.go`, table-driven, using the standard `testing` package; `go vet ./...` and
  `go test ./...` must pass.
- Goroutines always have a clear owner and a way to stop (context cancellation).
- Prefer the standard library; add a dependency only when the project already uses it.
