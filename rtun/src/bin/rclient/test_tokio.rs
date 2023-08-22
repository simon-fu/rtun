
use std::process::Stdio;

use anyhow::Result;
use bytes::BytesMut;
use rtun::async_stdin::async_std_in;
use tokio::{process::Command, io::{AsyncReadExt, AsyncWriteExt}};
use tracing::debug;

pub async fn test_tokio_main() -> Result<()> {
    // example_main().await
    client_main().await
}


async fn client_main() -> Result<()> {
    let pipe = Stdio::piped();
    let mut child = Command::new("bash")
    // .args([
    //     "-c",
    //     "for i in {1..4}; do echo \"Welcome $i/4\" && sleep 1;done",
    // ])
    // .arg("-i")
    .stdout(pipe)
    .stderr(Stdio::piped())
    .stdin(Stdio::piped())
    .spawn()?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    debug!("111");

    if let Some(stdout) = stdout {
        debug!("222");
        spawn_read_output_loop("stdout", stdout)?;
    }
    
    debug!("444");
    if let Some(stderr) = stderr {
        debug!("555");
        spawn_read_output_loop("stderr", stderr)?;
    }
    
    let child_task = tokio::spawn(async move {
        let status = child.wait().await
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
            let packet = ain.read().await?;
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


// async fn example_main() -> Result<()> {
//     use tokio::io::{BufReader, AsyncBufReadExt};

//     let mut cmd = Command::new("bash");
//     cmd.arg("-i");

//     // Specify that we want the command's standard output piped back to us.
//     // By default, standard input/output/error will be inherited from the
//     // current process (for example, this means that standard input will
//     // come from the keyboard and standard output/error will go directly to
//     // the terminal if this process is invoked from the command line).
//     cmd.stdout(Stdio::piped());
//     cmd.stderr(Stdio::piped());

//     let mut child = cmd.spawn()
//         .expect("failed to spawn command");

//     let stdout = child.stdout.take()
//         .expect("child did not have a handle to stdout");

//     let mut reader = BufReader::new(stdout).lines();
//     println!("111");

//     let stderr = child.stderr.take()
//         .expect("child did not have a handle to stderr");

//     tokio::spawn(async move {
//         let mut reader = BufReader::new(stderr).lines();
//         while let Some(Some(line)) = reader.next_line().await.ok() {
//             println!("err Line: {}", line);
//         }
//     });

//     // Ensure the child process is spawned in the runtime so it can
//     // make progress on its own while we await for any output.
//     tokio::spawn(async move {
//         let status = child.wait().await
//             .expect("child process encountered an error");

//         println!("child status was: {}", status);
//     });



//     println!("222");
//     while let Some(line) = reader.next_line().await? {
//         println!("out Line: {}", line);
//     }

//     println!("333");

//     Ok(())
// }
