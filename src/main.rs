#[tokio::main]
async fn main() {
    if let Err(error) = slack_codex::run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
