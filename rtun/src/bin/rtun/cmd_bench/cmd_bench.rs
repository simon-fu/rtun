use anyhow::Result;
use clap::Parser;

use crate::init_log_and_run;

use super::{bench_socks, udp_client, udp_server};

pub fn run(args: CmdArgs) -> Result<()> {
    init_log_and_run(do_run(args))?
}

async fn do_run(args: CmdArgs) -> Result<()> {
    match args.cmd {
        SubCmd::Socks(args) => bench_socks::run(args).await,
        SubCmd::UdpServer(args) => udp_server::run(args).await,
        SubCmd::UdpClient(args) => udp_client::run(args).await,
    }
}

#[derive(Parser, Debug)]
#[clap(name = "bench", author, about, version)]
pub struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Socks(bench_socks::CmdArgs),
    #[clap(name = "udp-server")]
    UdpServer(udp_server::CmdArgs),
    #[clap(name = "udp-client")]
    UdpClient(udp_client::CmdArgs),
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli_config::rewrite_legacy_bench_argv_for_test;

    use super::{CmdArgs, SubCmd};

    #[test]
    fn bench_cli_parses_udp_server_subcommand() {
        let args = CmdArgs::try_parse_from(["bench", "udp-server", "--listen", "0.0.0.0:9001"])
            .expect("parse bench udp-server");
        match args.cmd {
            SubCmd::UdpServer(cmd) => assert_eq!(cmd.listen, "0.0.0.0:9001"),
            _ => panic!("expected udp-server"),
        }
    }

    #[test]
    fn bench_cli_parses_udp_client_subcommand() {
        let args = CmdArgs::try_parse_from([
            "bench",
            "udp-client",
            "--target",
            "127.0.0.1:19001",
            "--mode",
            "reverse",
            "--time",
            "10",
            "--len",
            "1200",
        ])
        .expect("parse bench udp-client");
        match args.cmd {
            SubCmd::UdpClient(cmd) => {
                assert_eq!(cmd.target, "127.0.0.1:19001");
                assert_eq!(cmd.time, 10);
                assert_eq!(cmd.len, 1200);
                assert_eq!(cmd.mode.to_string(), "reverse");
            }
            _ => panic!("expected udp-client"),
        }
    }

    #[test]
    fn bench_cli_parses_socks_subcommand() {
        let args = CmdArgs::try_parse_from([
            "bench",
            "socks",
            "-s",
            "127.0.0.1:51080",
            "-a",
            "127.0.0.1",
            "-p",
            "12345",
        ])
        .expect("parse bench socks");
        match args.cmd {
            SubCmd::Socks(cmd) => {
                assert_eq!(cmd.socks, "127.0.0.1:51080");
                assert_eq!(cmd.target_addr, "127.0.0.1");
                assert_eq!(cmd.target_port, 12345);
            }
            _ => panic!("expected socks"),
        }
    }

    #[test]
    fn bench_cli_rewrites_legacy_bench_args_to_socks() {
        let rewritten = rewrite_legacy_bench_argv_for_test(vec![
            "rtun".to_string(),
            "bench".to_string(),
            "-s".to_string(),
            "127.0.0.1:51080".to_string(),
            "-a".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "12345".to_string(),
        ]);

        assert_eq!(
            rewritten,
            vec![
                "rtun",
                "bench",
                "socks",
                "-s",
                "127.0.0.1:51080",
                "-a",
                "127.0.0.1",
                "-p",
                "12345"
            ]
        );
    }

    #[test]
    fn bench_cli_rewrites_legacy_bench_args_to_socks_with_leading_legacy_flags() {
        let rewritten = rewrite_legacy_bench_argv_for_test(vec![
            "rtun".to_string(),
            "bench".to_string(),
            "-b".to_string(),
            "1024".to_string(),
            "--seconds".to_string(),
            "5".to_string(),
            "--socks".to_string(),
            "127.0.0.1:51080".to_string(),
            "-a".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "12345".to_string(),
        ]);

        assert_eq!(
            rewritten,
            vec![
                "rtun",
                "bench",
                "socks",
                "-b",
                "1024",
                "--seconds",
                "5",
                "--socks",
                "127.0.0.1:51080",
                "-a",
                "127.0.0.1",
                "-p",
                "12345"
            ]
        );
    }

    #[test]
    fn bench_cli_rewrites_legacy_bench_args_to_socks_with_equals_form() {
        let rewritten = rewrite_legacy_bench_argv_for_test(vec![
            "rtun".to_string(),
            "bench".to_string(),
            "--seconds".to_string(),
            "5".to_string(),
            "--socks=127.0.0.1:51080".to_string(),
            "-a".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "12345".to_string(),
        ]);

        assert_eq!(
            rewritten,
            vec![
                "rtun",
                "bench",
                "socks",
                "--seconds",
                "5",
                "--socks=127.0.0.1:51080",
                "-a",
                "127.0.0.1",
                "-p",
                "12345"
            ]
        );
    }
}
