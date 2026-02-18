use std::borrow::Cow;

use anyhow::{anyhow, bail, Result};
use chrono::Local;
use rtun::version::ver_brief;
use serde::{Deserialize, Serialize};

use crate::secret::token_gen;

pub const PUB_WS: &str = "/pub/ws";
pub const SUB_WS: &str = "/sub/ws";
pub const PUB_SESSIONS: &str = "/pub/sessions";

pub fn get_agent_from_url(url: &url::Url) -> Option<Cow<'_, str>> {
    let r = url.query_pairs().find(|x| x.0 == "agent").map(|x| x.1);
    r
}

pub fn make_pub_url(
    url: &mut url::Url,
    agent_name: Option<&str>,
    secret: Option<&str>,
) -> Result<()> {
    let path = url.path().trim_end_matches('/');
    url.set_path(format!("{}{}", path, PUB_WS).as_str());

    if let Some(agent_name) = agent_name {
        url.query_pairs_mut().append_pair("agent", agent_name);
    }

    let token: String = token_gen(secret, Local::now().timestamp_millis() as u64)?;
    url.query_pairs_mut().append_pair("token", token.as_str());

    url.query_pairs_mut().append_pair("ver", ver_brief());

    Ok(())
}

pub fn make_sub_url(
    url: &mut url::Url,
    agent_name: Option<&str>,
    secret: Option<&str>,
) -> Result<()> {
    let path = url.path().trim_end_matches('/');
    url.set_path(format!("{}{}", path, SUB_WS).as_str());

    if let Some(agent_name) = agent_name {
        url.query_pairs_mut().append_pair("agent", agent_name);
    }

    let token: String = token_gen(secret, Local::now().timestamp_millis() as u64)?;
    url.query_pairs_mut().append_pair("token", token.as_str());

    Ok(())
}

pub fn make_pub_sessions(url: &mut url::Url) -> Result<()> {
    let path = url.path().trim_end_matches('/');
    url.set_path(format!("{}{}", path, PUB_SESSIONS).as_str());
    Ok(())
}

pub fn make_ws_scheme(url: &mut url::Url) -> Result<()> {
    if url.scheme().eq_ignore_ascii_case("http") {
        url.set_scheme("ws")
            .map_err(|_e| anyhow!("set ws scheme fail"))?;
    } else if url.scheme().eq_ignore_ascii_case("https") {
        url.set_scheme("wss")
            .map_err(|_e| anyhow!("set ws scheme fail"))?;
    } else if url.scheme().eq_ignore_ascii_case("http")
        || url.scheme().eq_ignore_ascii_case("https")
    {
    } else {
        bail!("unsupport protocol [{}]", url.scheme())
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct AgentInfo {
    pub name: String,
    pub addr: String,
    pub expire_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PubParams {
    pub agent: Option<String>,
    pub token: String,
    pub expire_in: Option<u64>,
    pub instance_id: Option<String>,
    pub ver: Option<String>,
}

pub type SubParams = PubParams;
