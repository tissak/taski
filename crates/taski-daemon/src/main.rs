//! taski-daemon binary entry point. All real logic lives in the library crate so it
//! is unit/integration-testable; `main` just runs it and propagates any error.

fn main() -> anyhow::Result<()> {
    taski_daemon::run()
}
