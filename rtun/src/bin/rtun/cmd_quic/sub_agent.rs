use std::sync::Arc;
use bytes::BytesMut;
use clap::Parser;
use anyhow::{Result, Context};
use tracing::{info, debug};
use quinn::{Connection, RecvStream, SendStream};
use serde::{Serialize, Deserialize};
use rtun::{async_rt::spawn_with_name, huid::{gen_huid::gen_huid, HUId}};
use super::{quinn_util, xfer_util::{self, ResponseStatus}};

pub async fn run(args: CmdArgs) -> Result<()> {
    
    let endpoint = quinn_util::try_listen(&args.listen, args.https_key.as_deref(), args.https_cert.as_deref()).await?;

    info!("agent listening on [{}]", args.listen);

    let shared: AShared = Arc::new(Shared {
        args,
    });

    while let Some(conn) = endpoint.accept().await {
        let uid = gen_huid();
        let shared = shared.clone();
        spawn_with_name(uid.to_string(), async move {
            let remote_addr = conn.remote_address();
            debug!("connection incoming from [{remote_addr}]");
            let r = handle_connection(shared, conn, uid).await;
            debug!("finished with [{r:?}]");
        });
    }

    Ok(())
}

async fn handle_connection(shared: AShared, conn: quinn::Connecting, uid: HUId) -> Result<()> {
    
    let mut session = {
        let connection = conn.await.with_context(||"setup connection failed")?;


        let (tx, rx) = connection.accept_bi().await
        .with_context(||"accept first stream failed")?;
        
        Session {
            uid,
            conn: connection,
            tx,
            rx,
            rbuf: BytesMut::default(),
            wbuf: BytesMut::default(),
        }
    };

    {
        let req: AuthRequest = xfer_util::read_short_json(&mut session.rx, &mut session.rbuf).await?;

        let status = match &shared.args.secret {
            Some(_) => ResponseStatus::new(401, "Unauthorized".into()),
            None => ResponseStatus::ok(),
        };

        xfer_util::ser_short_json(&mut session.wbuf, &status)?;
        session.tx.write_all(&session.wbuf).await?;
        session.wbuf.clear();
    }

    loop {
        let (tx, rx) = session.conn.accept_bi().await
        .with_context(||"accept first stream failed")?;
    }

    Ok(())
}



#[derive(Debug, Serialize, Deserialize)]
struct AuthRequest {
    nonce: u64,
    token: String,
}

struct Session {
    uid: HUId,
    conn: Connection,
    tx: SendStream,
    rx: RecvStream,
    rbuf: BytesMut,
    wbuf: BytesMut,
}



type AShared = Arc<Shared>;

struct Shared {
    args: CmdArgs,
}


#[derive(Parser, Debug)]
#[clap(name = "agent", author, about, version)]
pub struct CmdArgs {
    #[clap(
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:19888",
    )]
    listen: String,

    #[clap(
        long = "https-cert",
        long_help = "https cert file",
    )]
    https_cert: Option<String>,

    #[clap(
        long = "https-key",
        long_help = "http key file",
    )]
    https_key: Option<String>,

    #[clap(
        long = "secret",
        long_help = "access secret",
    )]
    secret: Option<String>,
}
