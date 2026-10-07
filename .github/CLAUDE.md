<purpose>
# .github/
Repository-level GitHub configuration, currently containing the automated contribution gate.
</purpose>

<dependencies>
## Dependencies
GitHub Actions, Rust stable toolchain, and the Cargo workspace. See `workflows/CLAUDE.md`.
</dependencies>

<consumed-by>

## Consumed By
Static import scan (`.github`): no in-repo consumers found.

</consumed-by>

<data-flow>
## Data Flow
- Push and pull request events enter the workflows.
- CI results leave as repository check statuses.
</data-flow>

<subdirectories>
## Subdirectories
| Directory | Purpose |
|-----------|---------|
| `workflows/` | GitHub Actions definitions for formatting, linting, builds, tests, and release daemon validation. |
</subdirectories>