#!/usr/bin/env python3
import argparse
import signal
import socket
import time
from datetime import datetime


running = True


def ts() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]


def on_signal(_sig, _frame) -> None:
    global running
    running = False


def now_ms() -> int:
    return int(time.time() * 1000)


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Send UDP probe packets every interval and print echo results."
    )
    p.add_argument("--target-ip", required=True, help="remote target ip")
    p.add_argument("--target-port", type=int, default=1234, help="remote target port")
    p.add_argument(
        "--interval-ms",
        type=int,
        default=1000,
        help="send interval in milliseconds (default: 1000)",
    )
    p.add_argument(
        "--timeout-ms",
        type=int,
        default=800,
        help="recv timeout in milliseconds (default: 800)",
    )
    p.add_argument(
        "--max-consecutive-timeouts",
        type=int,
        default=0,
        help=(
            "exit after this many consecutive recv timeouts, "
            "0 means disable (default: 0)"
        ),
    )
    p.add_argument(
        "--count",
        type=int,
        default=0,
        help="number of probes, 0 means infinite",
    )
    p.add_argument(
        "--source-port",
        type=int,
        default=0,
        help="local source port, 0 means ephemeral",
    )
    p.add_argument("--prefix", default="probe", help="payload prefix")
    return p.parse_args()


def main() -> None:
    global running
    args = parse_args()

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", args.source_port))
    sock.settimeout(args.timeout_ms / 1000.0)

    target = (args.target_ip, args.target_port)
    print(
        f"{ts()} source={sock.getsockname()} target={target} "
        f"interval={args.interval_ms}ms timeout={args.timeout_ms}ms",
        flush=True,
    )

    sent = 0
    recv = 0
    rtts = []
    seq = 0
    consecutive_timeouts = 0

    while running:
        if args.count > 0 and seq >= args.count:
            break

        t0 = now_ms()
        payload = f"{args.prefix}-{seq}-{t0}".encode("utf-8")
        sent += 1
        try:
            sock.sendto(payload, target)
            data, addr = sock.recvfrom(65535)
            t1 = now_ms()
            rtt = t1 - t0
            recv += 1
            rtts.append(rtt)
            consecutive_timeouts = 0
            text = data.decode("utf-8", errors="replace")
            print(
                f"{ts()} ok seq={seq} rtt_ms={rtt} from={addr} recv={text}",
                flush=True,
            )
        except socket.timeout:
            consecutive_timeouts += 1
            print(
                f"{ts()} timeout seq={seq} wait={args.timeout_ms}ms "
                f"consecutive={consecutive_timeouts}",
                flush=True,
            )
            if (
                args.max_consecutive_timeouts > 0
                and consecutive_timeouts >= args.max_consecutive_timeouts
            ):
                print(
                    f"{ts()} stop reason=consecutive_timeouts "
                    f"count={consecutive_timeouts} "
                    f"threshold={args.max_consecutive_timeouts}",
                    flush=True,
                )
                break
        except Exception as exc:  # pylint: disable=broad-except
            print(f"{ts()} error seq={seq} err={exc}", flush=True)

        seq += 1
        time.sleep(max(0.0, args.interval_ms / 1000.0))

    lost = sent - recv
    loss_rate = (lost / sent * 100.0) if sent > 0 else 0.0
    if rtts:
        print(
            f"{ts()} summary sent={sent} recv={recv} lost={lost} "
            f"loss={loss_rate:.2f}% min={min(rtts)}ms max={max(rtts)}ms "
            f"avg={sum(rtts)/len(rtts):.2f}ms",
            flush=True,
        )
    else:
        print(
            f"{ts()} summary sent={sent} recv={recv} lost={lost} "
            f"loss={loss_rate:.2f}%",
            flush=True,
        )


if __name__ == "__main__":
    main()
