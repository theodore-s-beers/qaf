// A small, platform-independent network client for sandbox boundary tests.
use std::{env, net::{SocketAddr, TcpStream}, time::Duration};
fn main() -> std::io::Result<()> {
    let args: Vec<_> = env::args().collect();
    match args[1].as_str() {
        "tcp" => {
            let address: SocketAddr = args[2].parse().unwrap();
            TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
        }
        "unix" => { std::os::unix::net::UnixStream::connect(&args[2])?; }
        _ => panic!("unknown probe"),
    }
    Ok(())
}
