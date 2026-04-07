use clap::Parser;
use colored::Colorize;
use tracing::Level;

use aria2_core::init_logging;

#[derive(Parser, Debug)]
#[command(name = "aria2c")]
#[command(author, version, about = "aria2 - The ultra fast download utility", long_about = None)]
struct Args {
    #[arg(help = "URIs to download")]
    uris: Vec<String>,

    #[arg(short, long, help = "Output file name")]
    output: Option<String>,

    #[arg(short, long, help = "Directory to save downloaded files")]
    dir: Option<String>,

    #[arg(long, help = "Number of connections per server (default: 1)")]
    split: Option<u16>,

    #[arg(long, help = "Maximum number of connections (default: 5)")]
    max_connection_per_server: Option<u16>,

    #[arg(long, help = "Maximum download speed in bytes/sec")]
    max_download_limit: Option<u64>,

    #[arg(long, help = "Enable verbose logging")]
    verbose: bool,
}

fn print_banner() {
    println!("{}", "aria2-rust v0.1.0".green().bold());
    println!("{} {}", "Copyright:".blue(), "(C) 2024 aria2-rust contributors".white());
    println!();
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    print_banner();

    let log_level = if args.verbose { Level::DEBUG } else { Level::INFO };
    init_logging(log_level, None);

    if args.uris.is_empty() && args.output.is_none() {
        eprintln!("{}", "错误: 请提供下载URI或torrent文件路径".red());
        std::process::exit(1);
    }

    println!(
        "{} {}",
        "[INFO]".cyan(),
        format!("开始下载任务, 共 {} 个URI", args.uris.len()).white()
    );

    for uri in &args.uris {
        println!("  - {}", uri.yellow());
    }

    if let Some(output) = &args.output {
        println!("  输出文件: {}", output.green());
    }

    if let Some(dir) = &args.dir {
        println!("  保存目录: {}", dir.green());
    }

    println!();
    println!("{}", "注意: 核心功能正在开发中...".yellow());

    tracing::info!("aria2-rust 启动完成");
}
