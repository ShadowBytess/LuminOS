use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let mut socket = PathBuf::from("/run/luminos/init.sock");
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            match args.next() {
                Some(path) => socket = PathBuf::from(path),
                None => { eprintln!("luminctl: --socket requires a path"); std::process::exit(2); }
            }
        } else { command.push(arg); }
    }
    if command.is_empty() {
        eprintln!("usage: luminctl [--socket PATH] status [SERVICE] | start|stop|restart SERVICE | shutdown");
        std::process::exit(2);
    }
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(err) => { eprintln!("luminctl: cannot connect to {}: {err}", socket.display()); std::process::exit(1); }
    };
    if let Err(err) = writeln!(stream, "{}", command.join(" ")) {
        eprintln!("luminctl: send command: {err}"); std::process::exit(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut response = String::new();
    if let Err(err) = stream.read_to_string(&mut response) {
        eprintln!("luminctl: read response: {err}"); std::process::exit(1);
    }
    print!("{response}");
    if response.starts_with("ERR") { std::process::exit(1); }
    let _ = io::stdout().flush();
}
