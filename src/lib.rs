mod cli;
mod model;
mod runtime;
mod store;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
