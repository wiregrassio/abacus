<purpose>

# .github/workflows/

GitHub Actions CI gate: Rust formatting, linting, building, and testing.

</purpose>

<dependencies>

## Dependencies
- GitHub Actions runner image: `ubuntu-24.04`.
- `actions/checkout@v4` to retrieve repository source.
- `dtolnay/rust-toolchain@stable` to install the stable Rust toolchain plus `rustfmt` and Clippy.
- Cargo workspace commands and the repository's Rust packages, including `abacus-daemon`.

</dependencies>

<consumed-by>

## Consumed By
Consumed by GitHub Actions on push and PR events.

</consumed-by>

<data-flow>

## Data Flow
- GitHub push and pull-request events enter GitHub Actions and trigger the `check` job.
- The job checks out the repository and installs stable Rust tooling.
- Cargo commands consume workspace manifests and source files, producing formatting/lint results, build artifacts, test results, and a release build of `abacus-daemon`.
- Command exit status leaves the workflow as the CI pass/fail result reported on the commit or pull request.

</data-flow>

<known-hazards>

## Known Hazards
- MEDIUM: The workflow tracks the moving `stable` Rust toolchain rather than a pinned version; newly released compiler or Clippy behavior can fail previously passing revisions.
- MEDIUM: CI runs only on Linux (`ubuntu-24.04`); platform-specific build and runtime failures on other supported systems are not detected.
- LOW: The release build validates compilation of `abacus-daemon` but does not run release-mode tests.

</known-hazards>

<files>

## Files
| File | Purpose |
|---|---|
| `ci.yml` | Runs the Linux CI contribution gate on pushes and pull requests. |

</files>

<notes>

## Notes
The workflow intentionally treats warnings as failures through `cargo clippy ... -D warnings`, making lint cleanliness part of the merge gate.

</notes>

<reference>

## Reference

</reference>
