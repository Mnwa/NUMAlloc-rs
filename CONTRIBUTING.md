# Contributing to NUMAlloc

Thank you for your interest in contributing to NUMAlloc!

## Getting Started

1. Fork the repository and clone your fork.
2. Ensure you have a stable Rust toolchain installed: `rustup toolchain install stable`.
3. Build the project: `cargo build`.
4. Run the tests: `cargo test`.

## Development Workflow

### Before Submitting a Pull Request

All code must pass the following checks (these are enforced by CI):

1. **Formatting** — run `cargo fmt` and ensure there are no formatting issues:
   ```sh
   cargo fmt --all -- --check
   ```

2. **Linting** — run Clippy with warnings as errors:
   ```sh
   cargo clippy -- -D warnings
   ```

3. **Tests** — all tests must pass:
   ```sh
   cargo test
   ```

### Code Guidelines

- **`unsafe` blocks** must include a `// SAFETY:` comment explaining why the invariants hold.
- Prefer `NonNull<T>` over `*mut T` for non-null pointers.
- Use correct memory orderings for atomics: `Acquire` on loads, `Release` on stores, `AcqRel` on CAS.
- Minimize dependencies — this project intentionally only depends on `libc`.
- Inline aggressively on hot paths (`#[inline]`).
- Avoid heap allocations on alloc/dealloc paths (no `Vec`, `Box`, `String`).

### Pull Requests

- Keep PRs focused — one logical change per PR.
- Write a clear description of what the change does and why.
- Add tests for new functionality or bug fixes.
- Target the `main` branch.

### Reporting Issues

- Use GitHub Issues to report bugs or suggest features.
- Include steps to reproduce for bug reports.
- Mention your OS, Rust version, and hardware topology (number of NUMA nodes) if relevant.

## License

By contributing, you agree that your contributions will be licensed under the same license as the project.
