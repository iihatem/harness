use clap::Parser;

#[derive(Parser)]
#[command(
    name = "harness",
    version,
    about = "A hybrid local/frontier coding agent"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
