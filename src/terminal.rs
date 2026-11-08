use anyhow::{Result, ensure};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};

/// Nonblocking terminal I/O keeps both task input and approvals cancellable.
pub struct Terminal {
    file: File,
}

impl Terminal {
    pub fn open() -> std::io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .open("/dev/tty")?,
        })
    }

    pub async fn write(&self, text: &str) -> std::io::Result<()> {
        let mut remaining = text.as_bytes();
        let mut file = &self.file;
        while !remaining.is_empty() {
            match file.write(remaining) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub async fn read_line(&self, limit: usize) -> Result<Option<String>> {
        let mut bytes = Vec::new();
        let mut file = &self.file;
        loop {
            let mut byte = [0];
            match file.read(&mut byte) {
                Ok(0) if bytes.is_empty() => return Ok(None),
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    // Drain the entire line so leftovers cannot answer a later prompt.
                    if bytes.len() <= limit {
                        bytes.push(byte[0]);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(bytes.len() <= limit, "input exceeds {limit} bytes");
        Ok(Some(String::from_utf8(bytes)?))
    }
}

pub struct Human;

impl crate::permissions::Approver for Human {
    async fn approve(&self, request: &serde_json::Value) -> Result<crate::permissions::Decision> {
        use crate::permissions::Decision;
        let Ok(terminal) = Terminal::open() else {
            eprintln!("Broader access denied: no interactive terminal is available.");
            return Ok(Decision::Deny);
        };
        // JSON escaping prevents model-controlled terminal control sequences.
        terminal.write(&format!(
            "\nRun this operation WITHOUT the sandbox, with your user permissions?\n{}\nType allow (once), deny [default], or cancel: ",
            serde_json::to_string_pretty(request)?
        )).await?;
        let answer = terminal.read_line(32).await?.unwrap_or_default();
        Ok(match answer.trim() {
            "allow" => Decision::AllowOnce,
            "cancel" => Decision::Cancel,
            _ => Decision::Deny,
        })
    }
}

#[cfg(test)]
impl Terminal {
    pub fn pair() -> (Self, std::os::unix::net::UnixStream) {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let file = File::from(std::os::fd::OwnedFd::from(server));
        (Self { file }, client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_reads_do_not_steal_later_input_and_long_lines_are_drained() {
        let (terminal, mut client) = Terminal::pair();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), terminal.read_line(32))
                .await
                .is_err()
        );
        client
            .write_all("héllo\nthis line is too long\nallow\n".as_bytes())
            .unwrap();
        assert_eq!(
            terminal.read_line(32).await.unwrap().as_deref(),
            Some("héllo")
        );
        assert!(terminal.read_line(4).await.is_err());
        assert_eq!(
            terminal.read_line(32).await.unwrap().as_deref(),
            Some("allow")
        );
        drop(client);
        assert!(terminal.read_line(32).await.unwrap().is_none());
    }
}
