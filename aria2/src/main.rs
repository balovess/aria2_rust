//! aria2-rust CLI entry point.
//!
//! Uses clap derive API for argument parsing. aria2's optional
//! `--help[=TAG|KEYWORD]` action is rendered here before `App::run` so its
//! selector can be preserved. The `completions` subcommand is also handled
//! early and exits before the download engine starts.

use aria2::app::App;
use aria2::app::cli::{CliArgs, Commands, render_help};
use aria2::app::update_check;
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
    if let Some(Commands::Completions { shell }) = &cli.command {
        let mut cmd = CliArgs::command();
        let bin_name = "aria2c";
        clap_complete::generate(*shell, &mut cmd, bin_name, &mut std::io::stdout());
        std::process::exit(0);
    }

    if matches!(cli.command, Some(Commands::CheckUpdate)) {
        match update_check::check_now().await {
            Ok(Some(version)) => println!("A newer aria2-rust release is available: {version}"),
            Ok(None) => println!("aria2-rust is up to date."),
            Err(error) => {
                eprintln!("Update check failed: {error}");
                std::process::exit(1);
            }
        }
        std::process::exit(0);
    }

    if cli.general.repair_config || cli.general.reset_config {
        let config_path = cli
            .general
            .conf_path
            .as_deref()
            .expect("clap requires --conf-path for config maintenance");
        let result = if cli.general.repair_config {
            App::repair_config_file(config_path).map(|(backup, count)| {
                format!(
                    "Repaired {count} invalid configuration entr{}; backup: {}",
                    if count == 1 { "y" } else { "ies" },
                    backup.display()
                )
            })
        } else {
            App::reset_config_file(config_path)
                .map(|backup| format!("Reset configuration; backup: {}", backup.display()))
        };
        match result {
            Ok(message) => {
                println!("{message}");
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("Configuration maintenance failed: {error}");
                std::process::exit(1);
            }
        }
    }

    if cli.general.check_config.unwrap_or(false) {
        let no_conf = cli.general.no_conf.unwrap_or(false);
        let conf_path = cli
            .general
            .conf_path
            .as_ref()
            .and_then(|path| path.to_str());
        let mut app = App::new();
        match app.check_config(no_conf, conf_path).await {
            Ok(()) => {
                println!("Configuration is valid.");
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("Configuration is invalid: {error}");
                std::process::exit(1);
            }
        }
    }

    let exit_code = App::new().run(cli).await;
    std::process::exit(exit_code);
}
