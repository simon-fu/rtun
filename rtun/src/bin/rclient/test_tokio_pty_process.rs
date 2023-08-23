
/*
- 不能正常工作

*/
use std::{io::{Write, Read}, process::Command, pin::Pin, task::Poll};

use futures::{ready, StreamExt};
// use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
// use tokio::io::unix::AsyncFd;

use anyhow::{Result, Context};

use rtun::{term::{get_shell_program, async_input::{make_async_input, get_term_size}}, pty::PtyEvent};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};
// use tokio::io::{AsyncWriteExt, AsyncReadExt};
use tokio_pty_process::{AsyncPtyMaster, PtyMaster, CommandExt};

pub async fn test_main() -> Result<()> {
    
    let program: String = get_shell_program();

    let term_size = get_term_size()
        .with_context(||"get terminal size failed")?;

    tracing::debug!("shell_name [{}]", program);
    tracing::debug!("shell_name [{:?}]", term_size);

    let ptymaster = tokio::task::spawn_blocking( || {
        AsyncPtyMaster::open()
        .with_context(||"new pty fail")
        // pty_process::Pty::new()
        // .with_context(||"new pty fail")
    }).await??;

    tracing::debug!("created pty");
    
    let _child = Command::new(program.as_str())
    .arg("-i")
    .spawn_pty_async(&ptymaster)
    .with_context(||"failed to launch shell")?;

    tracing::debug!("launch program");

    let _r = ptymaster.resize(term_size.rows, term_size.cols)?;
    tracing::debug!("init size pty");

    // let mut out_buf = vec![0; 1024];
    // let mut pty = ptymaster;
    // let n = pty.read(&mut out_buf[..]).with_context(||"init read pty failed")?;
    // tracing::debug!("init read bytes {}", n);
    // let ptymaster = pty;

    let mut pty = AsyncPtyStream::new(ptymaster)?;


    // let mut pty = tokio::task::spawn_blocking( || {
    //     pty_process::Pty::new()
    //     .with_context(||"new pty fail")
    // }).await??;

    // let pts = pty.pts()
    //     .with_context(||"new pts fail")?;

    // // pty.resize(pty_process::Size::new(24, 80))
    // pty.resize(pty_process::Size::new(term_size.rows, term_size.cols))
    //     .with_context(||"resize pty fail")?;

    // let mut child = pty_process::Command::new(program)
    //     .arg("-i")
    //     .spawn(&pts)
    //     .with_context(||"spawn command fail")?;



    let mut out_buf = vec![0; 1024];
    let n = pty.read(&mut out_buf[..]).await.with_context(||"first read pty failed")?;

    // let mut out_buf = BytesMut::new();
    // let n = pty.read_buf(&mut out_buf).await.with_context(||"first read pty failed")?;

    tracing::debug!("first read bytes {}", n);

    // let mut ain = async_std_in();
    let mut fin = make_async_input()?;
    loop {
        tokio::select! {
            r = fin.next() => {
                let ev = r.with_context(||"reach stdin eof")??;
                match ev {
                    PtyEvent::StdinData(data) => {
                        pty.write_all(&data).await?;
                    },
                    PtyEvent::Resize(size) => {
                        pty.inner.get_ref().resize(size.rows, size.cols)?;
                    },
                }
            },
            r = pty.read(&mut out_buf) => {
                let _n = r.with_context(||"pty closed")?;
                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                stdout.write_all(&out_buf[..])?;
                stdout.flush()?;
                out_buf.clear();
            },
            // r = child.wait() => {
            //     let status = r?;
            //     bail!("program exit with {:?}", status)
            // }
        }
    }

}

pub struct AsyncPtyStream {
    inner: AsyncFd<AsyncPtyMaster>,
}

impl AsyncPtyStream {
    pub fn new(inner: AsyncPtyMaster) -> std::io::Result<Self> {
        let inner = AsyncFd::new(inner)?;
        Ok(Self {
            inner,
        })
    }

    pub async fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.inner.readable_mut().await?;
            tracing::debug!("read 111");
            match guard.try_io(|inner| {
                tracing::debug!("read 222");
                let r = inner.get_mut().read(out);
                tracing::debug!("read 333 {:?}", r);
                r
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.inner.writable_mut().await?;

            match guard.try_io(|inner| inner.get_mut().write(buf)) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn write_all(&mut self, mut buf: &[u8]) -> std::io::Result<()> { 
        while buf.len() > 0{
            let n = self.write(&buf[..]).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into())
            }
            buf = &buf[n..];
        }
        Ok(())
    }

}

impl AsyncRead for AsyncPtyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>
    ) -> Poll<std::io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_read_ready_mut(cx))?;

            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| inner.get_mut().read(unfilled)) {
                Ok(Ok(len)) => {
                    buf.advance(len);
                    return Poll::Ready(Ok(()));
                },
                Ok(Err(err)) => return Poll::Ready(Err(err)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for AsyncPtyStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8]
    ) -> Poll<std::io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready_mut(cx))?;

            match guard.try_io(|inner| inner.get_mut().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        // tcp flush is a no-op
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        // self.inner.get_ref().shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}
