#[tokio::main]
async fn main() -> wasmtime::Result<()> {
    host::direct_main().await
}
