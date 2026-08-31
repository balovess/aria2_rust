//! aria2-rust CLI entry point.
//!
//! Uses clap derive API for argument parsing. aria2's optional
//! `--help[=TAG|KEYWORD]` action is rendered here before `App::run` so its
//! selector can be preserved. The `completions` subcommand is also handled
//! early and exits before the download engine starts.

use aria2::app::App;
use aria2::app::cli::{CliArgs, Commands, render_help};
use aria2::app::update_check;
use aria2::app::{init, paths};
use clap::CommandFactory;
use clap::error::ErrorKind;

// mimalloc remains available for allocation-heavy deployments. The default
// Windows system allocator has a substantially smaller idle private footprint.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// aria2's main work is asynchronous I/O. A small fixed pool avoids reserving
// one runtime worker per logical CPU while retaining responsiveness when a
// protocol path performs a bounded synchronous operation.
fn main() {
    let result = std::thread::Builder::new()
        .name("aria2-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to create Tokio runtime")
                .block_on(async_main());
        })
        .expect("failed to create aria2 main thread")
        .join();
    if result.is_err() {
        std::process::exit(1);
    }
}

async fn async_main() {
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

    if cli.general.init {
        match init::initialize(init::InitRequest {
            profile: cli
                .general
                .profile
                .as_deref()
                .map(|profile| profile.parse())
                .transpose()
                .unwrap_or_else(|error| {
                    eprintln!("Initialization failed: {error}");
                    std::process::exit(1);
                }),
            state_dir: cli.general.state_dir,
            download_dir: cli.general.download_dir,
            non_interactive: cli.general.non_interactive,
            force: cli.general.force,
        }) {
            Ok(layout) => {
                println!("Initialization completed.");
                init::print_paths(&layout);
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("Initialization failed: {error}");
                std::process::exit(1);
            }
        }
    }

    if cli.general.show_paths {
        let profile = cli
            .general
            .profile
            .as_deref()
            .map(|profile| profile.parse())
            .transpose()
            .unwrap_or_else(|error| {
                eprintln!("Cannot resolve paths: {error}");
                std::process::exit(1);
            })
            .unwrap_or(paths::InitProfile::System);
        let layout =
            match paths::layout_for(profile, cli.general.state_dir, cli.general.download_dir) {
                Ok(layout) => layout,
                Err(error) => {
                    eprintln!("Cannot resolve paths: {error}");
                    std::process::exit(1);
                }
            };
        init::print_paths(&layout);
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
