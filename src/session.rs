//! Main module of rexpect: start new process and interact with it

use process::PtyProcess;
use reader::{NBReader, Regex};
pub use reader::ReadUntil;
use std::env;
use std::fs::{File, remove_file};
use std::io::LineWriter;
use std::process::Command;
use std::io::prelude::*;
use std::ops::{Deref, DerefMut};
use errors::*; // load error-chain

/// Interact with a process with read/write/signals, etc.
#[allow(dead_code)]
pub struct PtySession {
    process: PtyProcess,
    writer: LineWriter<File>,
    reader: NBReader,
    commandname: String, // only for debugging purposes now
}

/// Start a process in a tty session, write and read from it
///
/// # Example
///
/// ```
///
/// use rexpect::spawn;
/// # use rexpect::errors::*;
///
/// # fn main() {
///     # || -> Result<()> {
/// let mut s = spawn("cat", None)?;
/// s.send_line("hello, polly!")?;
/// let line = s.read_line()?;
/// assert_eq!("hello, polly!\r\n", line);
///         # Ok(())
///     # }().expect("test failed");
/// # }
/// ```
impl PtySession {
    /// sends string and a newline to process
    ///
    /// this is guaranteed to be flushed to the process
    /// returns number of written bytes
    pub fn send_line(&mut self, line: &str) -> Result<(usize)> {
        let mut len = self.send(line)?;
        len += self.writer
            .write(&['\n' as u8])
            .chain_err(|| "cannot write newline")?;
        Ok(len)
    }


    /// sends string to process. This may be buffered. You may use `flush()` after `send()`
    /// returns number of written bytes
    ///
    /// TODO: method to send ^C, etc.
    pub fn send(&mut self, s: &str) -> Result<(usize)> {
        self.writer
            .write(s.as_bytes())
            .chain_err(|| "cannot write line to process")
    }

    // wrapper around reader::read_until to give more context for errors
    fn exp(&mut self, needle: &ReadUntil) -> Result<String> {
        match self.reader.read_until(needle) {
            Ok(s) => Ok(s),
            Err(Error(ErrorKind::EOF(expected, got, _), _)) => {
                Err(ErrorKind::EOF(expected, got, self.process.status()).into())
            }
            Err(e) => Err(e),
        }
    }

    /// make sure all bytes written via `send()` are sent to the process
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().chain_err(|| "could not flush")
    }

    /// read one line (blocking!) and return line including newline (\r\n for tty processes)
    /// TODO: example on how to check for EOF
    pub fn read_line(&mut self) -> Result<String> {
        self.exp(&ReadUntil::String('\n'.to_string()))
    }

    pub fn exp_eof(&mut self) -> Result<()> {
        self.exp(&ReadUntil::EOF).and_then(|_| Ok(()))
    }

    pub fn exp_regex(&mut self, regex: &str) -> Result<()> {
        self.exp(&ReadUntil::Regex(Regex::new(regex).chain_err(|| "invalid regex")?))
            .and_then(|_| Ok(()))
    }

    pub fn exp_string(&mut self, needle: &str) -> Result<()> {
        self.exp(&ReadUntil::String(needle.to_string()))
            .and_then(|_| Ok(()))
    }

    pub fn exp_char(&mut self, needle: char) -> Result<()> {
        self.exp(&ReadUntil::String(needle.to_string()))
            .and_then(|_| Ok(()))
    }

    pub fn exp_any(&mut self, needles: Vec<ReadUntil>) -> Result<(String)> {
        self.exp(&ReadUntil::Any(needles))
    }
}

/// Start command in a pty session. Splits string at space and handles the rest as args
/// see spawn_command for more documentation.
pub fn spawn(program: &str, timeout: Option<u64>) -> Result<PtySession> {
    let command = if program.find(" ").is_some() {
        let mut parts = program.split(" ");
        let mut cmd = Command::new(parts.next().unwrap());
        cmd.args(parts);
        cmd
    } else {
        Command::new(program)
    };
    spawn_command(command, timeout)
}

/// starts command in background in a pty session (pty fork) and return a struct
/// with writer and buffered reader (for unblocking reads).
///
/// timeout: the number of milliseconds to wait at each `exp_*` command
pub fn spawn_command(command: Command, timeout: Option<u64>) -> Result<PtySession> {
    let commandname = format!("{:?}", &command);
    let process = PtyProcess::new(command)
        .chain_err(|| "couldn't start process")?;

    let f = process.get_file_handle();
    let writer = LineWriter::new(f.try_clone().chain_err(|| "couldn't open write stream")?);
    let reader = NBReader::new(f, timeout);
    Ok(PtySession {
           process: process,
           writer: writer,
           reader: reader,
           commandname: commandname,
       })
}

pub struct PtyBashSession {
    prompt: String,
    pty_session: PtySession,
}

impl PtyBashSession {
    fn wait_for_prompt(&mut self) -> Result<()> {
        self.pty_session.exp_string(&self.prompt)
    }
}

// make PtySession's methods available directly
impl Deref for PtyBashSession {
    type Target = PtySession;
    fn deref(&self) -> &PtySession { &self.pty_session }
}

impl DerefMut for PtyBashSession {
    fn deref_mut(&mut self) -> &mut PtySession { &mut self.pty_session }
}

impl Drop for PtyBashSession {
    fn drop(&mut self) {
        // if we leave that out, PtyProcess would try to kill the bash
        // which would not work, as a SIGTERM is not enough to kill bash
        self.pty_session.send_line("exit").expect("could not run `exit` on bash process");
    }
}


pub fn spawn_bash(timeout: Option<u64>) -> Result<PtyBashSession> {
    let mut dir = env::temp_dir();
    dir.push("rexpext_bashrc.sh");
    {
        let mut f = File::create(&dir).chain_err(|| "cannot create bashrc temp file")?;
        f.write(b"source /etc/bash.bashrc\n\
                  source ~/.bashrc\n\
                  PS1=\"$\"\n").chain_err(|| "cannot write to tmpfile")?;
    }
    let mut c = Command::new("bash");
    c.args(&["--rcfile", dir.to_str().unwrap()]);
    spawn_command(c, timeout).and_then(|mut p| {
        p.exp_char('$')?; // waiting for prompt
        let new_prompt = "[REXPECT_PROMPT>";
        p.send_line(&("PS1='".to_string() + new_prompt + "'"))?;
        remove_file(dir).chain_err(|| "cannot remove tmpfile")?;
        let mut pb = PtyBashSession { prompt: new_prompt.to_string(), pty_session: p };
        // PS1 does print another prompt, consume that as well
        pb.wait_for_prompt()?;
        Ok(pb)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_line() {
        || -> Result<()> {
            let mut s = spawn("cat", None)?;
            s.send_line("hans")?;
            assert_eq!("hans\r\n", s.read_line()?);
            let should = ::process::wait::WaitStatus::Signaled(s.process.child_pid,
                                                               ::process::signal::Signal::SIGTERM,
                                                               false);
            assert_eq!(should, s.process.exit()?);
            Ok(())
        }()
                .unwrap_or_else(|e| panic!("test_read_line failed: {}", e));
    }


    #[test]
    fn test_expect_eof_timeout() {
        || -> Result<()> {
            let mut p = spawn("sleep 3", Some(1000)).expect("cannot run sleep 3");
            match p.exp_eof() {
                Ok(_) => assert!(false, "should raise Timeout"),
                Err(Error(ErrorKind::Timeout(_, _, _), _)) => {}
                Err(_) => assert!(false, "should raise TimeOut"),

            }
            Ok(())
        }()
                .unwrap_or_else(|e| panic!("test_timeout failed: {}", e));
    }

    #[test]
    fn test_expect_eof_timeout2() {
        let mut p = spawn("sleep 1", Some(1100)).expect("cannot run sleep 1");
        assert!(p.exp_eof().is_ok(), "expected eof");
    }

    #[test]
    fn test_expect_string() {
        || -> Result<()> {
            let mut p = spawn("cat", Some(1000)).expect("cannot run cat");
            p.send_line("hello world!")?;
            p.exp_string("hello world!")?;
            p.send_line("hello heaven!")?;
            p.exp_string("hello heaven!")?;
            Ok(())
        }()
                .unwrap_or_else(|e| panic!("test_expect_string failed: {}", e));
    }

    #[test]
    fn test_expect_any() {
        || -> Result<()> {
            let mut p = spawn("cat", None).expect("cannot run cat");
            p.send_line("Hi")?;
            match p.exp_any(vec![ReadUntil::NBytes(3), ReadUntil::String("Hi".to_string())]) {
                Ok(s) => assert_eq!("Hi\r", s),
                Err(e) => assert!(false, format!("got error: {}", e)),
            }
            Ok(())
        }()
                .unwrap_or_else(|e| panic!("test_expect_any failed: {}", e));
    }

    #[test]
    fn test_bash() {
        || -> Result<()> {
            let mut p = spawn_bash(None)?;
            p.send_line("cd /tmp/")?;
            p.wait_for_prompt()?;
            p.send_line("pwd")?;
            assert_eq!("/tmp\r\n", p.read_line()?);
            Ok(())
        }().unwrap_or_else(|e| panic!("test_bash failed: {}", e));
    }
}
