


use quinn::Endpoint;
use std::{path::Path, sync::Arc};
use anyhow::{Result, Context, bail};


pub async fn try_listen(listen_str: &str, key_file: Option<&str>, cert_file: Option<&str>) -> Result<Endpoint> {

    let listen_addr = tokio::net::lookup_host(&listen_str).await
    .with_context(||format!("fail to lookup listen addr [{listen_str}]"))?
    .next()
    .with_context(||format!("got empty when lookup listen addr [{listen_str}]"))?;

    let r = try_load_quic_cert(
        key_file, 
        cert_file,
    ).await
    .with_context(|| "load quic key/cert file failed")?;

    let mut server_crypto = match r {
        Some(v) => v,
        None => gen_key_cert()?,
    };

    const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
    server_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    // server_crypto.key_log = Arc::new(rustls::KeyLogFile::new());

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));

    // server_config.use_retry(true);

    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());


    let endpoint = quinn::Endpoint::server(server_config, listen_addr)
    .with_context(||format!("failed to listen at [{listen_addr}]"))?;

    Ok(endpoint)
}

async fn try_load_quic_cert(key_file: Option<&str>, cert_file: Option<&str>) -> Result<Option<rustls::ServerConfig>> {
    if let (Some(key_path), Some(cert_path)) = (key_file, cert_file) {
        let key_path: &Path = key_path.as_ref();
        let cert_path: &Path = cert_path.as_ref();

        let key = tokio::fs::read(key_path).await.context("failed to read private key")?;
        let key = if key_path.extension().map_or(false, |x| x == "der") {
            rustls::PrivateKey(key)
        } else {
            let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
                .context("malformed PKCS #8 private key")?;
            match pkcs8.into_iter().next() {
                Some(x) => rustls::PrivateKey(x),
                None => {
                    let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                        .context("malformed PKCS #1 private key")?;
                    match rsa.into_iter().next() {
                        Some(x) => rustls::PrivateKey(x),
                        None => {
                            anyhow::bail!("no private keys found");
                        }
                    }
                }
            }
        };


        let cert_chain = tokio::fs::read(cert_path).await.context("failed to read certificate chain")?;
        let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
            vec![rustls::Certificate(cert_chain)]
        } else {
            rustls_pemfile::certs(&mut &*cert_chain)
                .context("invalid PEM-encoded certificate")?
                .into_iter()
                .map(rustls::Certificate)
                .collect()
        };

        let server_crypto = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;

        return Ok(Some(server_crypto))
    }


    if key_file.is_some() || cert_file.is_some() {
        if key_file.is_none() {
            bail!("no key file")
        }
        
        if cert_file.is_none() {
            bail!("no cert file")
        }
    }

    Ok(None)
}

fn gen_key_cert() -> Result<rustls::ServerConfig>{
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = cert.serialize_private_key_der();
    let cert = cert.serialize_der().unwrap();

    let key = rustls::PrivateKey(key);
    let cert = rustls::Certificate(cert);

    let server_crypto = rustls::ServerConfig::builder()
    .with_safe_defaults()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)?;

    Ok(server_crypto)
}
