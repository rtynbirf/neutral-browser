# Dependency policy

Every dependency must be:

- documented;
- version-locked;
- source-identifiable;
- auditable;
- removable or replaceable;
- free of mandatory runtime service dependence.

No dependency is trusted merely because it is popular.

For Rust builds, `Cargo.lock` is mandatory. Vendoring is the preferred offline/reproducible mode. CI/release builds must use `--locked`; fully offline builds may use `--frozen` once dependencies are locally available.
