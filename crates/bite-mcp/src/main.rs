//! bite — Rust control plane over a native Swift helper.

mod cli;
mod clients;
mod dispatch;
mod doctor;
mod helper;
mod mcp;
mod setup;

fn main() {
    let parsed = clap::Parser::parse();
    std::process::exit(dispatch::run(parsed));
}
