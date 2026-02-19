#!/usr/bin/env python3
from datetime import datetime
import socket
import time


def ts() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]


def main() -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    print(ts(), "source bind", sock.getsockname(), flush=True)
    sock.settimeout(1)

    for i in range(5):
        payload = f"pkt-{i}".encode()
        sock.sendto(payload, ("127.0.0.1", 20999))
        try:
            data, addr = sock.recvfrom(65535)
            print(ts(), "source recv", i, data, "from", addr, flush=True)
        except Exception as exc:
            print(ts(), "source miss", i, exc, flush=True)
        time.sleep(0.5)


if __name__ == "__main__":
    main()
