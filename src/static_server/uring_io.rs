//! Placeholder (replaced by the io_uring driver).

use super::OutBuf;

pub struct Listener;
pub struct Stream;
pub struct Writer;

impl Listener {
    pub fn new(l: std::net::TcpListener) -> Result<Listener, (std::io::Error, std::net::TcpListener)> {
        Err((std::io::Error::from(std::io::ErrorKind::Unsupported), l))
    }

    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

impl Stream {
    pub fn split(self) -> (tokio::io::BufReader<tokio::io::Empty>, Writer) {
        (tokio::io::BufReader::new(tokio::io::empty()), Writer)
    }
}

impl Writer {
    pub async fn send_all(&mut self, _b: OutBuf, _more: bool) -> std::io::Result<()> {
        Ok(())
    }

    pub fn shutdown(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    pub async fn send_file(&mut self, _f: &std::fs::File, _o: u64, _c: u64) -> std::io::Result<u64> {
        Ok(0)
    }
}
