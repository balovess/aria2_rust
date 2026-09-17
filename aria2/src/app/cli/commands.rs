use clap::Subcommand;

/// Subcommands supported by aria2c.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Open the interactive terminal user interface.
    Tui {
        /// TUI language (`en-US` or `zh-CN`; defaults to the system locale).
        #[arg(long = "language", visible_alias = "lang", value_name = "LOCALE")]
        language: Option<String>,
    },

    /// Generate shell completion scripts
    Completions {
        /// Shell type (bash, zsh, fish, elvish, powershell)
        shell: clap_complete::Shell,
    },

    /// Check for a newer aria2-rust release and exit
    CheckUpdate,
}
