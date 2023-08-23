use std::io::Write;
use anyhow::{Result, Context, bail};
use bytes::BytesMut;
use futures::StreamExt;
use rtun::{term::{get_shell_program, async_input::{make_async_input, get_term_size}}, pty::PtyEvent};
use tokio::io::{AsyncWriteExt, AsyncReadExt};

pub async fn test_main() -> Result<()> {
    
    let program: String = get_shell_program();

    let term_size = get_term_size()
        .with_context(||"get terminal size failed")?;

    tracing::debug!("shell_name [{}]", program);
    tracing::debug!("shell_name [{:?}]", term_size);

    let mut pty = tokio::task::spawn_blocking(
        || pty_process::Pty::new()
        .with_context(||"new pty fail")
    ).await??;

    let pts = pty.pts()
        .with_context(||"new pts fail")?;

    // pty.resize(pty_process::Size::new(24, 80))
    pty.resize(pty_process::Size::new(term_size.rows, term_size.cols))
        .with_context(||"resize pty fail")?;

    let mut child = pty_process::Command::new(program)
        .arg("-i")
        .spawn(&pts)
        .with_context(||"spawn command fail")?;

    // let (tx, mut rx) = async_pty_process::make_async_pty_process(
    //     &program, 
    //     &["-i"],
    //     term_size.rows, // 24,
    //     term_size.cols, //80,
    // ).await?;


    let mut out_buf = BytesMut::new();

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
                        pty.resize(pty_process::Size::new(size.rows, size.cols))?;
                    },
                }
            },
            r = pty.read_buf(&mut out_buf) => {
                let _n = r.with_context(||"pty closed")?;
                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                stdout.write_all(&out_buf[..])?;
                stdout.flush()?;
                out_buf.clear();
            },
            r = child.wait() => {
                let status = r?;
                bail!("program exit with {:?}", status)
            }
        }
    }

}



// pub async fn test_main() -> Result<()> {
    
//     let program: String = get_shell_program();

//     let (col, row) = crossterm::terminal::size()
//         .with_context(||"get terminal size failed")?;

//     tracing::debug!("shell_name [{}]", program);
//     tracing::debug!("shell_name [{:?}]", (row, col));

//     let (tx, mut rx) = async_pty_process::make_async_pty_process(
//         &program, 
//         &["-i"],
//         row, // 24,
//         col, //80,
//     ).await?;

//     let mut ain = async_std_in();
//     loop {
//         tokio::select! {
//             r = ain.read() => {
//                 let d = r?;
//                 tx.send_data(d).await.with_context(||"send stdin data to pty fail")?;
//             },
//             r = rx.recv() => {
//                 let d = r.with_context(||"pty closed")?;
//                 let stdout = std::io::stdout();
//                 let mut stdout = stdout.lock();
//                 stdout.write_all(&d[..]).unwrap();
//                 stdout.flush().unwrap();
//             }
//         }
//     }

// }

