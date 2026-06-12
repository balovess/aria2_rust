//! CLI display and help utilities
//!
//! This module handles all command-line interface display:
//! - Help message printing
//! - Version information
//! - Banner display
//! - Option mapping

use super::App;
use aria2_core::config::{OptionCategory, OptionRegistry};
use colored::Colorize;

impl App {
    /// Check if help or version flags are present and print them.
    pub(super) fn print_help_or_version(&self, args: &[String]) -> bool {
        for arg in args {
            if arg == "--help" || arg == "-h" {
                self.print_help();
                return true;
            }
            if arg == "--version" || arg == "-V" || arg == "-v" {
                self.print_version();
                return true;
            }
        }
        false
    }

    /// Print the application banner.
    pub(super) fn print_banner(&self) {
        println!("{}", "aria2-rust v0.1.0".green().bold());
        println!(
            "{} {}",
            "Copyright:".blue(),
            "(C) 2024 aria2-rust contributors".white()
        );
        println!();
    }

    /// Print the help message with all available options.
    fn print_help(&self) {
        let reg = OptionRegistry::new();

        println!(
            "{}",
            "aria2-rust - The ultra fast download utility"
                .green()
                .bold()
        );
        println!();
        println!("{}", "Usage:".yellow());
        println!("  aria2c [options] <URL> [URL]...");
        println!("  aria2c [options] -T <torrent_file>");
        println!();

        let categories = [
            (OptionCategory::General, "General Options"),
            (OptionCategory::HttpFtp, "HTTP/FTP Options"),
            (OptionCategory::BitTorrent, "BitTorrent Options"),
            (OptionCategory::Rpc, "RPC Options"),
            (OptionCategory::Advanced, "Advanced Options"),
        ];

        for (cat, title) in &categories {
            let opts = reg.by_category(*cat);
            if opts.is_empty() {
                continue;
            }
            println!("{}", title.yellow());
            for def in &opts {
                let short = match def.short_name() {
                    Some(c) => format!("-{}, ", c),
                    None => String::from("    "),
                };
                let type_hint = self.type_hint_for(def.opt_type());
                let default = self.default_str_for(def.default_value());
                print!("  {}--{}{}", short, def.name(), type_hint);
                if !default.is_empty() {
                    print!("  (default: {})", default);
                }
                println!();
                println!("      {}", def.description());
            }
            println!();
        }

        println!("  -h, --help                    Show this help message");
        println!("  -V, --version                 Show version information");
        println!();
        println!("Examples:");
        println!("  aria2c http://example.com/file.zip");
        println!("  aria2c -o output.iso http://example.com/image.iso");
        println!("  aria2c -d /downloads -s 4 http://example.com/large.bin");
    }

    /// Get type hint string for an option type.
    fn type_hint_for(&self, opt_type: aria2_core::config::OptionType) -> &'static str {
        use aria2_core::config::OptionType;
        match opt_type {
            OptionType::String | OptionType::Path => "=STR",
            OptionType::Integer | OptionType::Size => "=N",
            OptionType::Float => "=F",
            OptionType::Boolean => "",
            OptionType::List => "=LIST",
            OptionType::Enum => "=ENUM",
        }
    }

    /// Get default value string for display.
    fn default_str_for(&self, val: &aria2_core::config::OptionValue) -> String {
        use aria2_core::config::OptionValue;
        match val {
            OptionValue::None => String::new(),
            other => other.to_string(),
        }
    }

    /// Print version information.
    fn print_version(&self) {
        println!("aria2-rust {} (Rust)", env!("CARGO_PKG_VERSION"));
        println!("Built with Rust {}", env!("CARGO_PKG_RUST_VERSION"));
        println!();
        println!("Features: default,bittorrent,rpc,http");
        println!();
        println!("Supported protocols:");
        println!("  HTTP/HTTPS  ✅");
        println!("  FTP/SFTP    ✅");
        println!("  BitTorrent  ✅");
        println!("  Metalink    ✅");
        println!("  RPC(JSON/XML/WebSocket) ✅");
    }

    /// Map short option character to option name.
    pub(super) fn map_short_option(&self, c: char) -> Option<&'static str> {
        match c {
            // General
            'd' => Some("dir"),
            'o' => Some("out"),
            'i' => Some("input-file"),
            'q' => Some("quiet"),
            'l' => Some("log"),
            'L' => Some("log-level"),
            'n' => Some("dry-run"),
            'S' => Some("summary-interval"),
            'D' => Some("daemon"),
            // HttpFtp — timeouts & retries
            't' => Some("timeout"),
            'T' => Some("connect-timeout"),
            'm' => Some("max-tries"),
            'w' => Some("retry-wait"),
            // HttpFtp — connections
            's' => Some("split"),
            'x' => Some("max-connection-per-server"),
            'k' => Some("min-split-size"),
            'c' => Some("continue"),
            // HttpFtp — proxies
            'p' => Some("all-proxy"),
            'P' => Some("http-proxy"),
            'y' => Some("https-proxy"),
            'F' => Some("ftp-proxy"),
            'N' => Some("no-proxy"),
            // HttpFtp — headers & identity
            'U' => Some("user-agent"),
            'R' => Some("referer"),
            'H' => Some("header"),
            // HttpFtp — cookies
            'C' => Some("load-cookies"),
            'V' => Some("save-cookies"),
            // HttpFtp — SSL & file handling
            'b' => Some("check-certificate"),
            'E' => Some("ca-certificate"),
            'O' => Some("allow-overwrite"),
            // BitTorrent
            'g' => Some("seed-ratio"),
            'G' => Some("seed-time"),
            'B' => Some("bt-max-peers"),
            'h' => Some("listen-port"),
            // 'D' is already used for daemon above
            'X' => Some("bt-force-encryption"),
            'M' => Some("follow-torrent"),
            // RPC
            'e' => Some("enable-rpc"),
            'r' => Some("rpc-listen-port"),
            'I' => Some("rpc-secret"),
            // Advanced
            'j' => Some("max-concurrent-downloads"),
            'f' => Some("file-allocation"),
            'z' => Some("stop"),
            'A' => Some("max-overall-download-limit"),
            'Q' => Some("max-download-limit"),
            'W' => Some("max-overall-upload-limit"),
            'K' => Some("max-upload-limit"),
            'Z' => Some("disk-cache"),
            'Y' => Some("piece-length"),
            // Aliases
            'v' => Some("verbose"),
            _ => None,
        }
    }
}
