<purpose>
# workflows/
GitHub Actions workflow definitions for the repository's contribution gate.
</purpose>

<dependencies>
## Dependencies
GitHub Actions: `actions/checkout@v4`, `dtolnay/rust-toolchain@stable`. Cargo workspace packages and toolchain components: rustfmt, Clippy.
</dependencies>

<consumed-by>

## Consumed By
Static import scan (`workflows`): no in-repo consumers found.

</consumed-by>

<data-flow>
## Data Flow
- Push and pull request events enter GitHub Actions.
- The Ubuntu runner checks out the repository and installs the stable Rust toolchain.
- Cargo format, lint, build, test, and release-daemon build results determine CI status.
</data-flow>

<files>
## Files
| File | Purpose |
|------|---------|
| `ci.yml` | Runs formatting, warning-denied Clippy, workspace builds and tests, plus the release `abacus-daemon` build. |
</files>