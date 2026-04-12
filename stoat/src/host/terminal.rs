use std::io;

pub trait TerminalHost: Send {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    fn kill(&mut self) -> io::Result<()>;
}

pub(crate) struct PtyTerminal {
    writer: Box<dyn io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyTerminal {
    pub(crate) fn new(
        writer: Box<dyn io::Write + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    ) -> Self {
        Self { writer, child }
    }
}

impl TerminalHost for PtyTerminal {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.kill().map_err(io::Error::other)
    }
}
