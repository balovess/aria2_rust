mod app;
use app::App;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let exit_code = App::new().run(&args).await;
    std::process::exit(exit_code);
}
