use std::io;

use lamzu_hid::{Monitor, detect_connection};

fn main() -> io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => println!("USB: {:?}", detect_connection()?),
        Some("--sample") => {
            let mut monitor = Monitor::open()?;
            let result = monitor.refresh();
            println!("{:#?}", monitor.snapshot());
            result?;
        }
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: status [--sample]",
            ));
        }
    }
    Ok(())
}
