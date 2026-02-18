use anyhow::{bail, Result};
use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
// use hex_literal::hex;

type HmacSha256 = Hmac<Sha256>;

pub fn token_gen(secret: Option<&str>, nonce: u64) -> Result<String> {
    Secret::try_new(secret)?.gen_token(nonce)
}

pub fn token_verify(secret: Option<&str>, token: &str) -> Result<()> {
    Secret::try_new(secret)?.verify_token(token)
}

pub struct Secret {
    secret: Vec<u8>,
    buf: Vec<u8>,
}

impl Secret {
    pub fn try_new(secret: Option<&str>) -> Result<Self> {
        let secret = secret.unwrap_or("tKwC83s8");

        Ok(Self {
            // mac: HmacSha256::new_from_slice(secret.as_bytes())?,
            secret: secret.as_bytes().into(),
            buf: vec![0; 1024],
        })
    }

    pub fn gen_token(&mut self, nonce: u64) -> Result<String> {
        let input = nonce.to_be_bytes();

        let mac_bytes = {
            let mut mac = HmacSha256::new_from_slice(self.secret.as_ref())?;
            mac.update(&input[..]);
            mac.finalize().into_bytes()
        };

        self.buf[..8].clone_from_slice(&input[..]);
        self.buf[8..8 + mac_bytes.len()].clone_from_slice(&mac_bytes[..]);
        let encoded = general_purpose::STANDARD_NO_PAD.encode(&self.buf[..8 + mac_bytes.len()]);

        // let mut encoded = String::new();
        // general_purpose::STANDARD_NO_PAD.encode_string(input, &mut encoded);
        // general_purpose::STANDARD_NO_PAD.encode_string(mac_bytes, &mut encoded);

        Ok(encoded)
    }

    pub fn verify_token(&mut self, token: &str) -> Result<()> {
        // self.buf.clear();

        let len = general_purpose::STANDARD_NO_PAD.decode_slice(token, &mut self.buf)?;
        if len < 8 {
            bail!("token too short")
        }

        let nonce = &self.buf[0..8];
        let mac_input = &self.buf[8..len];

        let mac_output = {
            let mut mac = HmacSha256::new_from_slice(self.secret.as_ref())?;
            mac.update(nonce);
            mac.finalize().into_bytes()
        };

        if &mac_output[..] != mac_input {
            bail!("invalid token")
        }

        Ok(())
    }
}

#[test]
fn test() {
    {
        let token = Secret::try_new(None).unwrap().gen_token(123).unwrap();
        Secret::try_new(None)
            .unwrap()
            .verify_token(token.as_str())
            .unwrap();
    }

    {
        let secret = Some("it is demo secret");
        let token = Secret::try_new(secret).unwrap().gen_token(321).unwrap();
        Secret::try_new(secret)
            .unwrap()
            .verify_token(token.as_str())
            .unwrap();

        let r = Secret::try_new(None).unwrap().verify_token(token.as_str());
        assert!(r.is_err(), "{r:?}");

        let r = Secret::try_new(Some("wrong secret"))
            .unwrap()
            .verify_token(token.as_str());
        assert!(r.is_err(), "{r:?}");
    }
}
