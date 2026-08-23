//! aria2-rust CLI entry point.
//!
//! Uses clap derive API for argument parsing. aria2's optional
//! `--help[=TAG|KEYWORD]` action is rendered here before `App::run` so its
//! selector can be preserved. The `completions` subcommand is also handled
//! early and exits before the download engine starts.

use aria2::app::App;
use aria2::app::cli::{CliArgs, Commands, render_help};
use clap::CommandFactory;
use clap::error::ErrorKind;

// Use mimalloc as the global allocator for better multi-threaded throughput.
// On Windows, the system allocator has higher lock contention under concurrent
// allocations; mimalloc's per-thread caches significantly reduce this.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let cli = match CliArgs::try_parse() {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                print!("{}", error);
                std::process::exit(0);
            }
            _ => {
                eprint!("{}", error);
                std::process::exit(1);
            }
        },
    };

    if let Some(request) = cli.help.as_ref() {
        print!("{}", render_help(request));
        std::process::exit(0);
    }

    // Handle `completions <SHELL>` subcommand early — print completion script
    // to stdout and exit 0 without starting the download engine.
    if let Some(Commands::Completions { shell }) = cli.command {
        let mut cmd = CliArgs::command();
        let bin_name = "aria2c";
        clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
        std::process::exit(0);
    }

    let exit_code = App::new().run(cli).await;
    aria2_core::shutdown_logging();
    std::process::exit(exit_code);
}
