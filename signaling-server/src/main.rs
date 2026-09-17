#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let addr = std::env::var("SIGNALING_ADDR").unwrap_or_else(|_| "0.0.0.0:9876".to_owned());
    println!("[signaling] listening on {addr}");
    signaling_server::run(&addr).await
}
