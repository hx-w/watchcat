# Contributing

Thank you for helping make interrupted agent work safer and more reliable.

## Development setup

```bash
cargo test --all-targets --locked
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo package --locked
```

Pull requests should include tests and update the changelog when behavior is
user-visible. Keep dependencies focused and justify new runtime dependencies in
the pull request.

## Adding a provider

1. Implement the `Provider` trait in a module under `src/providers/`.
2. Register construction in `build_providers` without leaking provider checks
   into `WatchEngine`.
3. Translate native state into the generic models; do not add provider checks
   to `WatchEngine`.
4. Document authentication, session discovery, supported failure codes, and
   any experimental APIs.
5. Add parser tests, engine integration tests with a fake provider, and a
   read-only live smoke procedure.

Retry classifications are security-sensitive. A new retryable error requires
evidence that it cannot represent approval, authentication, billing, quota,
context, policy, or user-decision failures.

## Compatibility

Do not rewrite state files from an unknown future schema version. New fields
must have safe defaults. Renaming a provider, command, state key, or config
field requires a migration and a changelog entry.

## Release process

1. Update `CHANGELOG.md` and the version in `Cargo.toml`.
2. Run the full local verification commands above.
3. Merge to `main`, then push the matching `vMAJOR.MINOR.PATCH` tag.
4. The release workflow builds five native targets, creates SHA-256 checksums,
   publishes a GitHub Release, and tests both installers against that release.
