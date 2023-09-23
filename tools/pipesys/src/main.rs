use crate::cmd::{init_logger, Args};
use anyhow::Result;
use clap::Parser;

mod cmd;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logger(args.log_level);
    cmd::run(args).await
}
