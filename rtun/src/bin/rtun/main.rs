/* 
TODO:
    - 支持 cmd exec 
    - 支持 agent 和 client 鉴权
    
    - 支持 环境变量 设置 日志级别
    - 支持 agent pub 断开重试
    = 支持 socks client 断开重试
    
    - client 通过 bridge 连接 agent， agent结束后， client 端卡死
    - agent 端 shell 结束后要发送 0 包给对方，所有 channel 在关闭时都应该发送 0 包
    - AsynInput 实现 poll_next ， 以便于统一使用 stream 接口
    - 动态调整 终端 大小
    - 合并 resize 事件 : AsynInput 里已经实现
    - 调用系统默认 shell，不是写死 bash: 修改 get_shell_program 即可
    - 支持 wss
*/

use anyhow::Result;
use rtun::async_rt;
use clap::Parser;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

pub mod cmd_shell;

pub mod cmd_agent;

pub mod cmd_socks;

pub mod terminal;

pub mod client_ch_pty;

pub mod rest_proto;

pub mod client_utils;

// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "rtun", author, about, version)]
struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Shell(cmd_shell::CmdArgs),
    Socks(cmd_socks::CmdArgs),
    Agent(cmd_agent::CmdArgs),
}

fn main() -> Result<()> {

    init_log();
    let args = CmdArgs::parse();

    let r = async_rt::run_multi_thread(async move { 
        return match args.cmd {
            SubCmd::Shell(args) => cmd_shell::run(args).await,
            SubCmd::Socks(args) => cmd_socks::run(args).await,
            SubCmd::Agent(args) => cmd_agent::run(args).await,
        }
    });
    tracing::debug!("main finished with {:?}", r);
    r??;
    Ok(())
}

fn init_log() {

    let filter = if cfg!(debug_assertions) {
        "rtun=debug".into()
    } else {
        EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
    };
        
    tracing_subscriber::fmt()
    .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    .with_env_filter(filter)
    // .with_env_filter("rtun=debug,rserver=debug")
    .init();
}
