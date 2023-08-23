
use anyhow::{Result, Context};
use async_process::{Command, Stdio};
use bytes::BytesMut;
use futures::{AsyncWriteExt as FAsyncWriteExt, StreamExt};
use rtun::{async_stdin::async_std_in, term::get_shell_program};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use tokio_util::compat::FuturesAsyncReadCompatExt;

pub async fn test_main() -> Result<()> {
    let program = get_shell_program();
    let mut child = Command::new(program)
    // .args([
    //     "-c",
    //     "for i in {1..4}; do echo \"Welcome $i/4\" && sleep 1;done",
    // ])
    .arg("-i")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .stdin(Stdio::piped())
    .spawn()?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    debug!("test_async_process");
    debug!("111");

    if let Some(stdout) = stdout {
        debug!("222");
        spawn_read_output_loop("stdout", stdout.compat())?;
    }
    
    debug!("444");
    if let Some(stderr) = stderr {
        debug!("555");
        spawn_read_output_loop("stderr", stderr.compat())?;
    }
    
    let child_task = tokio::spawn(async move {
        let status = child.status().await
            .expect("child process encountered an error");

        println!("child status was: {}", status);
    });

    debug!("777");
    if let Some(mut stdin) = stdin {
        debug!("888");
        let mut ain = async_std_in();
        loop {
            debug!("999");
            // tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            // debug!("999-000");
            let packet = ain.next().await
            .with_context(||"stdin eof")??;
            debug!("999-111 {:?}", std::str::from_utf8(&packet[..]));
            // if sub process exit, here got "Broken pipe" error
            stdin.write_all(&packet[..]).await?;
            debug!("999-222");
        }
    }


    child_task.await?;

    debug!("000");
    // let r = child.wait().await?;
    // debug!("exit status {:?}", r);
    Ok(())
    

    // Ok(())
}


fn spawn_read_output_loop<R>(rtype: &'static str, mut reader: R) -> Result<()> 
where
    R: AsyncReadExt + Sized + Unpin + Send + 'static,
{
    let _r = tokio::spawn(async move {
        let mut buf = BytesMut::new();
        let _r = read_output_loop(rtype, &mut reader, &mut buf).await;
    });

    Ok(())
}

async fn read_output_loop<R>(rtype: &'static str, reader: &mut R, buf: &mut BytesMut) -> Result<()> 
where
    R: AsyncReadExt + Sized + Unpin,
{
    let r = do_read_output_loop(rtype, reader, buf).await;
    if let Err(e) = &r {
        debug!("read output loop error {:?}", e)
    }
    r
}

async fn do_read_output_loop<R>(rtype: &'static str, reader: &mut R, buf: &mut BytesMut) -> Result<()> 
where
    R: AsyncReadExt + Sized + Unpin,
{
    loop {
        buf.clear();
        let n = reader.read_buf(buf).await?;
        
        if n == 0 {
            debug!("{} got eof", rtype);
            break;
        }

        if let Ok(s) = std::str::from_utf8(&buf[..n]) {
            print!("{}", s);
            let _r = tokio::io::stdout().flush().await;
        }
        buf.clear();
    }
    Result::<()>::Ok(())
}


