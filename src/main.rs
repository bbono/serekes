mod bot;
mod common;
mod engine;
mod strategy;
#[tokio::main]
async fn main() {
    bot::run().await;
}
