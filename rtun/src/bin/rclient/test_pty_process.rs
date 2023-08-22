use std::io::Write;
use anyhow::{Result, Context};
use rtun::{async_stdin::async_std_in, async_pty_process};


pub async fn test_main() -> Result<()> {
    
    let shell_name = std::env::var("SHELL").unwrap_or("bash".to_string());
    let (col, row) = crossterm::terminal::size()
        .with_context(||"get terminal size failed")?;

    tracing::debug!("shell_name [{}]", shell_name);
    tracing::debug!("shell_name [{:?}]", (row, col));

    let (tx, mut rx) = async_pty_process::make_async_pty_process(
        &shell_name, 
        &["-i"],
        row, // 24,
        col, //80,
    ).await?;

    let mut ain = async_std_in();
    loop {
        tokio::select! {
            r = ain.read() => {
                let d = r?;
                tx.send_data(d).await.with_context(||"send stdin data to pty fail")?;
            },
            r = rx.recv() => {
                let d = r.with_context(||"pty closed")?;
                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                stdout.write_all(&d[..]).unwrap();
                stdout.flush().unwrap();
            }
        }
    }

}

