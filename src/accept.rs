//! The page face's accept loop (ledger #753).

use std::io;

use tokio::net::{TcpListener, TcpStream};

/// Accept connections on `listener`, handing each to `on_connection`.
pub async fn serve_until_error<F>(listener: &TcpListener, mut on_connection: F) -> io::Error
where
    F: FnMut(TcpStream),
{
    loop {
        match listener.accept().await {
            Ok((sock, _)) => on_connection(sock),
            Err(e) => eprintln!("http accept: {e}"),
        }
    }
}
