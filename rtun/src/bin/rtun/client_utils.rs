
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
                    tracing::debug!("select agent [{}]", agent.name);
                },
            }
            
            make_ws_scheme(&mut url)?;
    }
    else {
        bail!("unsupport protocol [{}]", url.scheme())
    };

    Ok(url)
}

async fn query_and_select_agent(url: &url::Url) -> Result<AgentInfo> {
    let mut agents = get_agents(url)
    .await?;

    if agents.len() == 0 {
        bail!("agent list empty")
    }

    Ok(agents.swap_remove(0))
}

async fn get_agents(url: &url::Url) -> Result<Vec<AgentInfo>> {
    let mut url = url.clone();
    make_pub_sessions(&mut url)?;
    reqwest::get(url)
    .await?
    .json()
    .await
    .map_err(|e|e.into())
}

