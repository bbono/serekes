mod bot;
mod common;
mod engine;
mod strategy;
mod types;
#[tokio::main]
async fn main() {
    bot::run().await;
}
