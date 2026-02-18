
use std::collections::HashMap;

use anyhow::{Result, Context, bail};

use crate::rest_proto::{make_sub_url, make_ws_scheme, AgentInfo, make_pub_sessions};


pub async fn client_select_url(url_str: &str, agent: Option<&str>, secret: Option<&str>) -> Result<url::Url> {
    let mut url = url::Url::parse(url_str)
    .with_context(||"invalid url")?;

    if url.scheme().eq_ignore_ascii_case("ws") 
        || url.scheme().eq_ignore_ascii_case("wss") {
        
    } else if url.scheme().eq_ignore_ascii_case("http") 
        || url.scheme().eq_ignore_ascii_case("https") {
            match agent {
                Some(agent) => {
                    make_sub_url(&mut url, Some(agent), secret)?;
                },
                None => {
                    let agent = query_and_select_agent(&url).await?;
                    make_sub_url(&mut url, Some(agent.name.as_str()), secret)?;
                },
            }
            
            make_ws_scheme(&mut url)?;
    }
    else {
        bail!("unsupport protocol [{}]", url.scheme())
    };

    Ok(url)
}

pub async fn query_new_agents(url: &url::Url, agent_regex: &regex::Regex, exist: &mut HashMap<String, AgentInfo>) -> Result<Vec<AgentInfo>> {
    if url.scheme().eq_ignore_ascii_case("http") 
    || url.scheme().eq_ignore_ascii_case("https") {
        let mut agents = get_agents(url).await?;
        // tracing::debug!("get_agents success [{agents:?}]");

        let mut pos = 0;
        for nn in 0..agents.len() {
            let name = &agents[nn].name;
            if !agent_regex.is_match(name) {
                continue;
            }

            match exist.get(name) {
                Some(_exist) => {},
                None => {
                    exist.insert(name.clone(), agents[nn].clone());
                    if nn > pos {
                        agents.swap(nn, pos);
                    }
                    pos += 1;
                },
            }
        }
        agents.resize(pos, AgentInfo {
            name: "non-exist".into(),
            addr: "".into(),
            expire_at: 0,
            instance_id: None,
            ver: None,
        });
        Ok(agents)
    } else {
        bail!("unsupport scheme [{}]", url.scheme())
    }
}

async fn query_and_select_agent(url: &url::Url) -> Result<AgentInfo> {
    let mut agents = get_agents(url)
    .await?;

    if agents.len() == 0 {
        bail!("agent list empty")
    }

    agents.sort_by(|a, b|b.expire_at.cmp(&a.expire_at));
    tracing::debug!("sorted agents {agents:?}");

    Ok(agents.swap_remove(0))
}

pub async fn get_agents(url: &url::Url) -> Result<Vec<AgentInfo>> {
    let mut url = url.clone();
    make_pub_sessions(&mut url)?;
    reqwest::get(url)
    .await?
    .json()
    .await
    .map_err(|e|e.into())
}
