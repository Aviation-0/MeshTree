use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "meshtree", version, about = "MeshTree workspace coordinator")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!("meshtree {}", env!("CARGO_PKG_VERSION"));
}
