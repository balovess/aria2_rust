//! CLI argument definitions using clap derive API.
//!
//! This module defines the `CliArgs` struct that replaces the hand-rolled
//! parser in `cli_options.rs`. All option names and short forms mirror the
//! `OptionRegistry` in `aria2-core`, with conflict resolution:
//! - `-h` → help (aria2_original)
//! - `-v` → version (aria2_original)
//! - `-V` → check-integrity (aria2_original)
//! - `-L` → listen-port (Rust additive alias)
//! - `--save-cookies` has no short form (matching aria2_original)
//!
//! # Boolean option semantics (`--opt[=true|false]`)
//!
//! Upstream aria2 registers every boolean option through `BooleanOptionHandler`
//! with `OptionHandler::OPT_ARG`, which `OptionParser` maps onto `getopt_long`'s
//! `optional_argument`. That yields exactly four accepted spellings:
//!
//! | Spelling         | Result                                                |
//! |------------------|-------------------------------------------------------|
//! | `--opt`          | `true` (value omitted → `A2_V_TRUE`)                   |
//! | `--opt=true`     | `true`                                                 |
//! | `--opt=false`    | `false`                                                |
//! | `--opt=<other>`  | error: "must be either 'true' or 'false'."             |
//!
//! Critically, `--opt true` (space separated) is **not** consumed as a value:
//! `optional_argument` only recognises the `=` form, so `true` falls through to
//! the positional URI list. `aria2c --continue http://host/f.bin` therefore
//! still downloads `http://host/f.bin`.
//!
//! The clap equivalent is:
//!
//! ```ignore
//! #[arg(
//!     long = "continue",
//!     num_args(0..=1),
//!     require_equals = true,
//!     default_missing_value = "true",
//!     value_name = "true|false"
//! )]
//! pub continue_dl: Option<bool>,
//! ```
//!
//! * `num_args(0..=1)` makes the value optional.
//! * `require_equals = true` reproduces `optional_argument`: the value must be
//!   attached with `=`, so clap never swallows the following whitespace
//!   separated argument.
//! * `default_missing_value = "true"` supplies the implicit `true`.
//! * clap's built-in `bool` value parser accepts only the literals `true` and
//!   `false`, matching `BooleanOptionHandler::parseArg`.
//!
//! Every boolean is `Option<bool>` rather than `bool` so that the merge step in
//! [`super::config`] can distinguish three states:
//!
//! * `None` — the user did not mention the option; keep the config-file,
//!   environment, or registry-default value.
//! * `Some(true)` — explicitly enabled on the command line.
//! * `Some(false)` — explicitly disabled on the command line; this must override
//!   a `continue=true` line in `aria2.conf`.
//!
//! A plain `bool` collapses the first and last case, which silently dropped
//! `--continue=false` style overrides.

use std::ffi::OsString;
use std::str::FromStr;

use aria2_core::config::{OptionRegistry, OptionValue};
use clap::{Arg, ArgAction, CommandFactory, Parser};
use colored::Colorize;

use super::App;

mod advanced;
mod bittorrent;
mod commands;
mod general;
mod http_ftp;
mod rpc;

pub use advanced::AdvancedArgs;
pub use bittorrent::BitTorrentArgs;
pub use commands::Commands;
pub use general::GeneralArgs;
pub use http_ftp::HttpFtpArgs;
pub use rpc::RpcArgs;

/// The optional argument accepted by aria2's `-h`/`--help` option.
///
/// This is deliberately kept separate from the typed configuration option
/// parser. Help is a process-level command, not a value that is applied to a
/// download task or exposed through RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelpRequest {
    /// Show the default basic help section.
    Basic,
    /// Show help selected by an aria2 tag (`#http`) or option-name keyword.
    Filter(String),
}

impl FromStr for HelpRequest {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            Ok(Self::Basic)
        } else {
            Ok(Self::Filter(value.to_owned()))
        }
    }
}

// =========================================================================
// Top-level CLI struct
// =========================================================================

/// Command-line arguments for the aria2-compatible binary.
///
/// `name = "aria2c"` preserves the established executable entry point for
/// existing clients. The displayed version still comes from this product's
/// package metadata, and the binary remains `aria2c` via `[[bin]]` in
/// `aria2/Cargo.toml`.
#[derive(Parser, Debug)]
#[command(
    name = crate::identity::PRODUCT_NAME,
    version = crate::identity::PRODUCT_VERSION,
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
    about = "aria2-rust - The ultra fast download utility",
    long_about = None,
    before_help = "Start here:\n  Download one URL:       aria2c https://example.com/file.zip\n  Choose folder/name:      aria2c -d DIR -o NAME URL\n  Download a URL list:     aria2c -i urls.txt\n  Resume a download:       aria2c -c URL\n  Download a torrent:      aria2c file.torrent\n  Download a magnet:       aria2c 'magnet:?xt=...'\n\nINPUT\n  URL, magnet URI, .torrent, or .metalink file path can be used as input.\n  A .metalink input requires a build with the `metalink` feature.\n\nCommon next steps:\n  --help=#basic       Show the options most users need first\n  --help=#http        Show HTTP/HTTPS options\n  --help=#bittorrent  Show BitTorrent options\n  --help=OPTION       Search options by name, for example --help=proxy\n  --init              Create a persistent configuration and state layout",
    after_help = "Examples:\n  aria2c https://example.com/file.zip\n  aria2c -d C:\\Downloads -o file.zip https://example.com/file.zip\n  aria2c -x 16 -s 16 https://example.com/large.iso\n  aria2c -i urls.txt\n  aria2c --conf-path C:\\Apps\\Aria2\\aria2.conf https://example.com/file.zip\n  aria2c tui --language=zh-CN\n  aria2c --rpc-url http://127.0.0.1:6800/jsonrpc --rpc-token SECRET\n\nOption values can use either --option=value or --option value. Boolean options accept --option, --option=true, and --option=false.\nUse --help=KEYWORD or --help=#basic, --help=#advanced, or --help=#http to narrow the help output. Use `aria2c tui` for interactive controls."
)]
pub struct CliArgs {
    /// General options
    #[command(flatten)]
    pub general: GeneralArgs,

    /// HTTP/FTP options
    #[command(flatten)]
    pub http_ftp: HttpFtpArgs,

    /// BitTorrent options
    #[command(flatten)]
    pub bittorrent: BitTorrentArgs,

    /// RPC options
    #[command(flatten)]
    pub rpc: RpcArgs,

    /// Advanced options
    #[command(flatten)]
    pub advanced: AdvancedArgs,

    /// aria2-compatible version action using the aria2-rust product version.
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,

    /// Original aria2 help action (`-h`, `--help[=TAG|KEYWORD]`).
    ///
    /// `require_equals` is important here: aria2's optional argument is only
    /// consumed in the `--help=value` form, so `--help URI` leaves `URI` as a
    /// positional input instead of treating it as a help filter.
    #[arg(
        short = 'h',
        long = "help",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "",
        value_name = "TAG|KEYWORD"
    )]
    pub help: Option<HelpRequest>,

    /// Verbose output
    #[arg(
        long = "verbose",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub verbose: Option<bool>,

    /// Disable colored output
    #[arg(
        long = "no-color",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_color: Option<bool>,

    /// Download URIs (HTTP/HTTPS/FTP/FTPS URLs or .torrent/.metalink file paths)
    #[arg(value_name = "URI")]
    pub uris: Vec<String>,

    /// Subcommands
    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl CliArgs {
    /// Parse process arguments after preserving getopt's attached `-hVALUE`
    /// optional-argument form. Clap otherwise treats the remainder as a short
    /// option cluster (`-h` + `-V` + ...), which changes aria2's argv contract.
    pub fn parse() -> Self {
        <Self as Parser>::parse_from(normalize_short_help_args(std::env::args_os()))
    }

    /// Parse process arguments without letting clap terminate the process.
    /// The binary maps ordinary parse failures to aria2's nonzero CLI error
    /// path while retaining successful help/version exits.
    pub fn try_parse() -> Result<Self, clap::Error> {
        <Self as Parser>::try_parse_from(normalize_short_help_args(std::env::args_os()))
    }

    /// Testable equivalent of [`Parser::try_parse_from`] with aria2 argv
    /// normalization applied before clap sees the tokens.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
        <Self as Parser>::try_parse_from(normalize_short_help_args(args))
    }
}

fn normalize_short_help_args<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    args.into_iter()
        .map(|arg| {
            let Some(value) = arg.to_str() else {
                return arg;
            };
            let suffix = value.strip_prefix("-h").unwrap_or_default();
            if suffix.starts_with('=') {
                // getopt's short optional argument includes this '='. The
                // original option_processing.cc then truncates at the first
                // '=' and falls back to the basic help section.
                OsString::from("-h")
            } else if !suffix.is_empty() {
                OsString::from(format!("-h={suffix}"))
            } else {
                arg
            }
        })
        .collect()
}

/// Render help without entering the application lifecycle.
///
/// The original executable treats help filters as an output concern. Keeping
/// that behaviour here prevents a help selector from being applied as a
/// configuration option and gives tests a pure seam for the process-level
/// command. Keyword filtering is exact on the public long option name; tag
/// filtering uses the CLI's explicit option headings and the original tag
/// names that have a direct Rust representation.
pub fn render_help(request: &HelpRequest) -> String {
    // Keep the compatibility executable name in help usage while the version
    // action itself is rendered with the independent product identity.
    let mut command = CliArgs::command().name("aria2c");

    let filter = match request {
        HelpRequest::Basic => "#basic",
        HelpRequest::Filter(raw_filter) => normalize_help_filter(raw_filter),
    };
    let registry = OptionRegistry::new();
    command = command.mut_args(|arg| {
        let visible = arg.get_long().is_some() || arg.get_short().is_some();
        if visible && matches_help_filter(&arg, filter) {
            add_default_to_help(arg, &registry)
        } else {
            arg.hide(true)
        }
    });

    command.render_help().to_string()
}

/// Add registry defaults to help text without configuring Clap defaults.
///
/// Clap defaults would change the merge contract: an absent CLI option must
/// remain absent so config-file and environment values can win. The registry
/// is therefore used only while rendering help.
fn add_default_to_help(mut arg: Arg, registry: &OptionRegistry) -> Arg {
    let Some(name) = arg.get_long() else {
        return arg;
    };
    let Some(definition) = registry.get(name) else {
        return arg;
    };
    let Some(default) = definition.parse_default_value() else {
        return arg;
    };
    let Some(help) = arg.get_help().map(ToString::to_string) else {
        return arg;
    };

    let mut details = Vec::new();
    if !definition.allowed_values().is_empty() {
        details.push(format!(
            "possible values: {}",
            definition.allowed_values().join(", ")
        ));
    }
    if let Some(range) = format_help_range(definition) {
        details.push(format!("range: {range}"));
    }
    if let Some(unit) = format_help_unit(name, definition.opt_type()) {
        details.push(format!("unit: {unit}"));
    }
    details.push(format!("default: {}", format_help_value(&default)));

    arg = arg.help(format!("{help} [{}]", details.join("] [")));
    arg
}

fn format_help_range(definition: &aria2_core::config::OptionDef) -> Option<String> {
    match (definition.min, definition.max) {
        (Some(min), Some(max)) => Some(format!("{min}..={max}")),
        (Some(min), None) => Some(format!(">={min}")),
        (None, Some(max)) => Some(format!("<= {max}")),
        (None, None) => None,
    }
}

fn format_help_unit(
    name: &str,
    option_type: aria2_core::config::OptionType,
) -> Option<&'static str> {
    if option_type == aria2_core::config::OptionType::Size {
        return Some("bytes (K/M/G/T suffixes accepted)");
    }

    const SECOND_OPTIONS: &[&str] = &[
        "auto-save-interval",
        "connect-timeout",
        "dns-timeout",
        "retry-wait",
        "save-session-interval",
        "server-stat-timeout",
        "startup-idle-time",
        "summary-interval",
        "timeout",
    ];
    SECOND_OPTIONS.contains(&name).then_some("seconds")
}

fn format_help_value(value: &OptionValue) -> String {
    match value {
        OptionValue::Str(value) if value.is_empty() => "empty".to_string(),
        OptionValue::Str(value) if value.contains(char::is_whitespace) => {
            format!("'{value}'")
        }
        _ => value.to_string(),
    }
}

fn normalize_help_filter(raw_filter: &str) -> &str {
    let filter = raw_filter.strip_prefix("--").unwrap_or(raw_filter);
    filter
        .split_once('=')
        .map_or(filter, |(keyword, _)| keyword)
}

fn matches_help_filter(arg: &Arg, filter: &str) -> bool {
    let name = arg.get_long().unwrap_or_default();
    if let Some(tag) = filter.strip_prefix('#') {
        return matches_help_tag(arg, name, tag);
    }

    name.contains(filter)
}

fn matches_help_tag(arg: &Arg, name: &str, tag: &str) -> bool {
    match tag {
        "all" => true,
        "basic" => BASIC_HELP_OPTIONS.contains(&name),
        "advanced" => arg.get_help_heading() == Some("Advanced options"),
        "http" | "https" => {
            arg.get_help_heading() == Some("HTTP/FTP options")
                && (tag == "http" || name.contains("https") || name == "check-certificate")
        }
        "ftp" => arg.get_help_heading() == Some("HTTP/FTP options") && name.contains("ftp"),
        "bittorrent" => arg.get_help_heading() == Some("BitTorrent options"),
        "metalink" => name.contains("metalink") || name == "select-file",
        "rpc" => arg.get_help_heading() == Some("RPC options"),
        "cookie" => name.contains("cookie"),
        "hook" => name.contains("hook") || name.starts_with("on-"),
        "file" => name.contains("file") || matches!(name, "dir" | "out"),
        "checksum" => name.contains("check") || name.contains("hash"),
        "experimental" => name == "enable-utp",
        "deprecated" => name == "dht-message-path",
        "help" => name == "help",
        _ => false,
    }
}

// This list follows aria2's basic help surface. Options not represented by
// the current CLI are naturally omitted from the generated output.
const BASIC_HELP_OPTIONS: &[&str] = &[
    "allow-piece-length-change",
    "always-resume",
    "auto-save-interval",
    "bt-max-peers",
    "check-integrity",
    "continue",
    "dht-listen-addr6",
    "dht-listen-port",
    "dir",
    "enable-dht",
    "enable-dht6",
    "file-allocation",
    "ftp-passwd",
    "force-sequential",
    "ftp-pasv",
    "ftp-user",
    "help",
    "index-out",
    "input-file",
    "listen-port",
    "load-cookies",
    "log",
    "max-connection-per-server",
    "max-concurrent-downloads",
    "max-overall-upload-limit",
    "max-upload-limit",
    "max-tries",
    "min-split-size",
    "no-netrc",
    "out",
    "parameterized-uri",
    "quiet",
    "save-session",
    "save-session-interval",
    "seed-ratio",
    "seed-time",
    "show-files",
    "split",
    "timeout",
    "torrent-file",
    "metalink-file",
    "update-check",
    "update-check-interval-days",
    "http-passwd",
    "http-user",
    "user-agent",
    "version",
];

// =========================================================================
// Banner display (kept here for colored output integration)
// =========================================================================

pub(crate) fn product_banner_title() -> String {
    format!(
        "{} version {}",
        crate::identity::PRODUCT_NAME,
        crate::identity::PRODUCT_VERSION
    )
}

impl App {
    /// Print the application banner using the product identity.
    pub(super) fn print_banner(&self, output_to_stderr: bool) {
        let banner = format!(
            "{}\n{} {}\n\n",
            product_banner_title().green().bold(),
            "Copyright:".blue(),
            "(C) aria2-rust contributors".white()
        );
        if output_to_stderr {
            eprint!("{}", banner);
        } else {
            print!("{}", banner);
        }
    }
}

#[cfg(test)]
mod banner_tests {
    use super::product_banner_title;

    #[test]
    fn banner_uses_product_identity() {
        assert_eq!(
            product_banner_title(),
            format!(
                "{} version {}",
                crate::identity::PRODUCT_NAME,
                crate::identity::PRODUCT_VERSION
            )
        );
    }
}
