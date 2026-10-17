//! The accept loop of the widget HTTP server.

use std::net::{TcpListener, TcpStream};

/// A plain HTTP/1.1 server bound to one port.
pub struct HttpServer {
    port: u16,
    max_connections: usize,
}

impl HttpServer {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            max_connections: 128,
        }
    }

    /// Accepts connections until the process stops.
    pub fn listen_and_serve(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", self.port))?;
        for stream in listener.incoming().take(self.max_connections) {
            handle_connection(stream?);
        }
        Ok(())
    }
}

fn handle_connection(stream: TcpStream) {
    drop(stream);
}
