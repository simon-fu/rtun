#!/usr/bin/env python3
from datetime import datetime
import socket


def ts() -> str:
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]


def main() -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 10999))
    print(ts(), "target listen", sock.getsockname(), flush=True)

    while True:
        data, addr = sock.recvfrom(65535)
        print(ts(), "target recv from", addr, data, flush=True)
        sock.sendto(data, addr)


if __name__ == "__main__":
    main()
