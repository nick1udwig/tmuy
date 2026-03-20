mod cli;
mod model;
mod rpc;
mod runtime;
mod sandbox;
mod store;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
