//! aria2-rust CLI entry point.
//!
//! Uses clap derive API for argument parsing. The `--help` and `--version`
//! flags are handled by clap before `App::run` is called. The `completions`
//! subcommand is handled early here and exits before the download engine starts.

use aria2::app::App;
use aria2::app::cli::{CliArgs, Commands};
use clap::{CommandFactory, Parser};

// Use mimalloc as the global allocator for better multi-threaded throughput.
// On Windows, the system allocator has higher lock contention under concurrent
// allocations; mimalloc's per-thread caches significantly reduce this.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let cli = CliArgs::parse();

    // Handle `completions <SHELL>` subcommand early — print completion script
    // to stdout and exit 0 without starting the download engine.
    if let Some(Commands::Completions { shell }) = cli.command {
        let mut cmd = CliArgs::command();
        let bin_name = "aria2c";
        clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
        std::process::exit(0);
    }

    let exit_code = App::new().run(cli).await;
    std::process::exit(exit_code);
}
