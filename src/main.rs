use api_relay::{App, config::Config};

#[tokio::main]
async fn main() {
    let config = Config::from_env();
    let app = App::new(&config).await;

    if let (Some(addr), Some(public)) = (&config.public_bind_addr, app.public) {
        println!("Public API listening on {addr}");
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tokio::spawn(async move { axum::serve(listener, public).await.unwrap() });
    }

    println!("API Relay listening on {}", config.bind_addr);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await.unwrap();
    axum::serve(listener, app.lan).await.unwrap();
}
