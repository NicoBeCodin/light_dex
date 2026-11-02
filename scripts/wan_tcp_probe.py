#!/usr/bin/env python3
"""Measure a persistent TCP connection with application acknowledgments.

Run `server <bind-address> <port>` on one host, then
`client <server-address> <port>` on the other. Reverse the hosts for the
opposite direction. This probe sends at most a few megabytes per direction.
"""

import argparse
import socket
import statistics
import struct
import time


def receive_exact(connection: socket.socket, size: int) -> bytes:
    data = bytearray()
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise EOFError("connection closed during frame")
        data.extend(chunk)
    return bytes(data)


def serve(host: str, port: int) -> None:
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((host, port))
        listener.listen(1)
        listener.settimeout(60)
        print("READY", flush=True)
        connection, _ = listener.accept()
        with connection:
            connection.settimeout(10)
            while header := connection.recv(4):
                if len(header) < 4:
                    header += receive_exact(connection, 4 - len(header))
                size = struct.unpack("!I", header)[0]
                receive_exact(connection, size)
                connection.sendall(b"K")


def probe(host: str, port: int, rounds: int, pause: float) -> None:
    with socket.create_connection((host, port), timeout=10) as connection:
        connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        connection.settimeout(10)
        for size in (512, 8192, 16384, 24576, 32768):
            timings = []
            frame = struct.pack("!I", size) + b"x" * size
            for _ in range(rounds):
                start = time.monotonic_ns()
                connection.sendall(frame)
                if receive_exact(connection, 1) != b"K":
                    raise RuntimeError("wrong acknowledgment")
                timings.append((time.monotonic_ns() - start) / 1e6)
                time.sleep(pause)
            ordered = sorted(timings)
            p95 = ordered[int(0.95 * (len(ordered) - 1))]
            print(f"{size} bytes: p50={statistics.median(ordered):.1f}ms "
                  f"p95={p95:.1f}ms max={ordered[-1]:.1f}ms", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("server", "client"))
    parser.add_argument("host")
    parser.add_argument("port", type=int)
    parser.add_argument("--rounds", type=int, default=16)
    parser.add_argument("--pause", type=float, default=0.35)
    args = parser.parse_args()
    if args.mode == "server":
        serve(args.host, args.port)
    else:
        probe(args.host, args.port, args.rounds, args.pause)


if __name__ == "__main__":
    main()
