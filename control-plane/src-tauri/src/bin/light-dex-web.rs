#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    light_dex_control_plane_lib::web::serve().await
}
