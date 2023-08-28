
use anyhow::{Result, bail, anyhow};
use chrono::Local;
use serde::{ Serialize, Deserialize };

use crate::secret::token_gen;

pub const PUB_WS: &str = "/pub/ws";
pub const SUB_WS: &str = "/sub/ws";
pub const PUB_SESSIONS: &str = "/pub/sessions";

pub fn make_pub_url(url: &mut url::Url, agent_name: Option<&str>, secret: Option<&str>) -> Result<()> {
    let path = url.path().trim_end_matches('/');
    url.set_path(format!("{}{}", path, PUB_WS).as_str());

    if let Some(agent_name) = agent_name {
        url.query_pairs_mut().append_pair("agent", agent_name);    
    }

    let token: String = token_gen(secret, Local::now().timestamp_millis() as  u64)?;
    url.query_pairs_mut().append_pair("token", token.as_str());

    Ok(())
}

pub fn make_sub_url(url: &mut url::Url, agent_name: Option<&str>, secret: Option<&str>) -> Result<()>  {
    let path = url.path().trim_end_matches('/');
    url.set_path(format!("{}{}", path, SUB_WS).as_str());
    
    if let Some(agent_name) = agent_name {
        url.query_pairs_mut().append_pair("agent", agent_name);    
    }
    
    let token: String = token_gen(secret, Local::now().timestamp_millis() as  u64)?;
    url.query_pairs_mut().append_pair("token", token.as_str());
    
    Ok(())
}

pub fn make_pub_sessions(url: &mut url::Url, ) -> Result<()> {
    let path = url.path().trim_end_matches('/');
    url.set_path(format!("{}{}", path, PUB_SESSIONS).as_str());
    Ok(())
}

pub fn make_ws_scheme(url: &mut url::Url) -> Result<()> {
    if url.scheme().eq_ignore_ascii_case("http") {
        url.set_scheme("ws").map_err(|_e|anyhow!("set ws scheme fail"))?;
    } else if url.scheme().eq_ignore_ascii_case("https") {
        url.set_scheme("wss").map_err(|_e|anyhow!("set ws scheme fail"))?;
    } else if url.scheme().eq_ignore_ascii_case("http") || url.scheme().eq_ignore_ascii_case("https") {

    } else {
        bail!("unsupport protocol [{}]", url.scheme())
    }
    Ok(())
}


#[derive(Serialize, Deserialize, Debug)]
#[allow(dead_code)]
pub struct AgentInfo {
    pub name: String,
    pub addr: String,
}

#[derive(Debug, Deserialize)]
pub struct PubParams {
    pub agent: Option<String>,
    pub token: String,
}

pub type SubParams = PubParams;
