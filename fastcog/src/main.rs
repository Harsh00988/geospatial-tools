use anyhow::Result;
use clap::Parser;
use fastcog::{run, Args};

fn main() -> Result<()> {
    run(&Args::parse())
}
