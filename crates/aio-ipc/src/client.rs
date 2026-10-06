//! Blocking client: the pipe is opened like a file, so no async runtime is needed.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use crate::{PIPE_NAME, Request, Response, decode_line, encode_line};

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Client {
    reader: BufReader<File>,
    writer: File,
}

impl Client {
    /// Connects to the daemon, waiting briefly if all pipe instances are busy.
    pub fn connect() -> io::Result<Self> {
        let started = Instant::now();
        let file = loop {
            match OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
                Ok(f) => break f,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && started.elapsed() < CONNECT_TIMEOUT =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the aio daemon is not running",
                    ));
                }
                Err(e) => return Err(e),
            }
        };
        Ok(Self { writer: file.try_clone()?, reader: BufReader::new(file) })
    }

    pub fn request(&mut self, req: &Request) -> io::Result<Response> {
        self.writer.write_all(&encode_line(req))?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed the pipe"));
        }
        decode_line(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}
