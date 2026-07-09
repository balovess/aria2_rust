mod app;
pub mod constants;
use app::App;

#[tokio::main]
async fn main() {
    // Skip argv[0] (the program path). On Windows the executable path contains
    // backslashes, which detect() would otherwise turn into an http:// URL and
    // cause the app to download its own binary on every launch.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit_code = App::new().run(&args).await;
    std::process::exit(exit_code);
}
