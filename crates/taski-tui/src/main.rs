//! taski-tui binary entry point. All real logic lives in the library crate so it is
//! unit-testable and so the future unified launcher can call `taski_tui::run` from
//! its `taski tui` subcommand. `main` just runs it and propagates any error.

fn main() -> anyhow::Result<()> {
    taski_tui::run()
}
