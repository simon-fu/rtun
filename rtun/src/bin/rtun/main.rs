/* 
TODO:
    - 支持 cmd exec 
    - 支持 channel flow control 
    - 支持 agent pub 多个实例
    - 支持 client 连接多个 agent 实例 
    - 研究是否支持运行docker https://driverfarfar.jetbrains.space/p/main ，已用谷歌账号注册

Done: 
    - 支持 TERM 环境变量
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
use rtun::{async_rt, version::ver_full};
use clap::Parser;
use time::{macros::format_description, UtcOffset};
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt::{MakeWriter, time::OffsetTime}};

pub mod cmd_shell;

pub mod cmd_agent;

pub mod cmd_socks;

pub mod cmd_local;

pub mod cmd_udp;

pub mod cmd_nat4;

pub mod cmd_punch;

pub mod cmd_quic;

pub mod cmd_echo;

pub mod cmd_bench;

pub mod terminal;

pub mod client_ch_pty;

pub mod rest_proto;

pub mod client_utils;

pub mod secret;

// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "rtun", author, about, version=ver_full())]
struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Shell(cmd_shell::CmdArgs),
    Socks(cmd_socks::CmdArgs),
    Agent(cmd_agent::CmdArgs),
    Local(cmd_local::CmdArgs),
    Udp(cmd_udp::CmdArgs),
    Nat4(cmd_nat4::CmdArgs),
    Quic(cmd_quic::CmdArgs),
    Punch(cmd_punch::CmdArgs),
    Echo(cmd_echo::CmdArgs),
    Bench(cmd_bench::CmdArgs),
}

fn main() -> Result<()> {
    let args = CmdArgs::parse();
    match args.cmd {
        SubCmd::Shell(args) => cmd_shell::run(args),
        SubCmd::Socks(args) => cmd_socks::run(args),
        SubCmd::Agent(args) => cmd_agent::run(args),
        SubCmd::Local(args) => cmd_local::run(args),
        SubCmd::Udp(args) => cmd_udp::run(args),
        SubCmd::Nat4(args) => cmd_nat4::run(args),
        SubCmd::Quic(args) => cmd_quic::run(args),
        SubCmd::Punch(args) => cmd_punch::run(args),
        SubCmd::Echo(args) => cmd_echo::run(args),
        SubCmd::Bench(args) => cmd_bench::run(args),
    }

    // init_log();
    // tracing::info!("rtun {}", ver_full());
    // let args = CmdArgs::parse();

    // let r = async_rt::run_multi_thread(async move { 
    //     return match args.cmd {
    //         SubCmd::Shell(args) => cmd_shell::run(args).await,
    //         SubCmd::Socks(args) => cmd_socks::run(args).await,
    //         SubCmd::Agent(args) => cmd_agent::run(args).await,
    //         SubCmd::Local(args) => cmd_local::run(args).await,
    //         SubCmd::Udp(args) => cmd_udp::run(args).await,
    //     }
    // });
    // tracing::debug!("main finished with {:?}", r);
    // r??;
    // Ok(())
}

pub(crate) fn init_log_and_run<F: futures::Future>(future: F) -> Result<F::Output> {
    init_log();
    async_rt::run_multi_thread(future)
}
pub(crate) fn init_log() {
    init_log2(||std::io::stdout())
}

pub(crate) fn init_log2<W2>(w: W2) 
where
    W2: for<'writer> MakeWriter<'writer> + 'static + Send + Sync,
{

    // https://time-rs.github.io/book/api/format-description.html
    let fmts = format_description!("[hour]:[minute]:[second].[subsecond digits:3]");

    let offset = UtcOffset::current_local_offset().expect("should get local offset!");
    let timer = OffsetTime::new(offset, fmts);
    
    let filter = if cfg!(debug_assertions) {
        if let Ok(v) = std::env::var(EnvFilter::DEFAULT_ENV) {
            v.into()
        } else {
            "rtun=debug".into()
        }
    } else {
        EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
    };
        
    tracing_subscriber::fmt()
    .with_max_level(tracing::metadata::LevelFilter::DEBUG)
    .with_env_filter(filter)
    // .with_env_filter("rtun=debug,rserver=debug")
    .with_writer(w)
    .with_timer(timer)
    .with_target(false)
    .init();
}
