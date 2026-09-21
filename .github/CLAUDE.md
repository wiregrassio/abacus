<purpose>

# .github/

Repository automation: the Rust continuous-integration contribution gate.

</purpose>

<dependencies>

## Dependencies
- GitHub Actions on `ubuntu-24.04`.
- `actions/checkout@v4`.
- `dtolnay/rust-toolchain@stable` with Rustfmt and Clippy.
- Cargo workspace metadata, source code, tests, and the `abacus-daemon` package.

</dependencies>

<consumed-by>

## Consumed By
Consumed by GitHub Actions on push and PR events.

</consumed-by>

<data-flow>

## Data Flow
- Push and pull-request events enter the workflow under `workflows/`.
- Repository source is checked out, and the stable Rust toolchain is installed.
- Cargo formatting, linting, build, test, and daemon release-build commands inspect or compile the workspace.
- Command exit statuses become the CI result reported to the originating commit or pull request.

</data-flow>

<known-hazards>

## Known Hazards
- MEDIUM: The workflow tracks the moving `stable` Rust toolchain; compiler or Clippy releases can break previously passing revisions.
- MEDIUM: CI runs only on Linux (`ubuntu-24.04`), leaving platform-specific failures elsewhere undetected.
- LOW: The daemon release build checks compilation but does not execute tests in release mode.

</known-hazards>

<files>

## Files
| File | Purpose |
|------|---------|

</files>

<notes>

## Notes
Warnings are deliberately promoted to CI failures, making Clippy cleanliness part of the contribution contract.

</notes>

<reference>

## Reference

</reference>
