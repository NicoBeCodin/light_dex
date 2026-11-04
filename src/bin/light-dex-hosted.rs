#[path = "../hosted/mod.rs"]
mod hosted;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    hosted::run().await
}
