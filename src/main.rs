use clap::Parser;
use filesync::{ProgramArgs, run};

fn main() {
    let args = ProgramArgs::parse();
    println!("{}", run(args));
}
