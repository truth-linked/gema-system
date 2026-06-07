#!/usr/bin/env python3
import select
import socket
import sys
import threading
import time


def pipe(left, right):
    sockets = [left, right]
    try:
        while True:
            readable, _, _ = select.select(sockets, [], [], 30)
            if not readable:
                continue
            for src in readable:
                data = src.recv(65536)
                if not data:
                    return
                dst = right if src is left else left
                dst.sendall(data)
    finally:
        for sock in sockets:
            try:
                sock.close()
            except OSError:
                pass


def connect_upstream(guest_host, guest_port, deadline_seconds=10):
    deadline = time.monotonic() + deadline_seconds
    last_error = None
    while time.monotonic() < deadline:
        try:
            return socket.create_connection((guest_host, guest_port), timeout=2)
        except OSError as exc:
            last_error = exc
            time.sleep(0.2)
    raise last_error or TimeoutError("upstream connect deadline reached")


def handle(client, guest_host, guest_port):
    try:
        upstream = connect_upstream(guest_host, guest_port)
    except OSError as exc:
        print(f"[gema-router] upstream unavailable {guest_host}:{guest_port}: {exc}", flush=True)
        try:
            client.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        client.close()
        return
    pipe(client, upstream)


def main():
    if len(sys.argv) != 4:
        print("usage: gema-tcp-router.py <host_port> <guest_host> <guest_port>", file=sys.stderr)
        return 64
    host_port = int(sys.argv[1])
    guest_host = sys.argv[2]
    guest_port = int(sys.argv[3])
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("0.0.0.0", host_port))
    server.listen(256)
    print(f"[gema-router] listening 0.0.0.0:{host_port} -> {guest_host}:{guest_port}", flush=True)
    while True:
        client, _ = server.accept()
        threading.Thread(target=handle, args=(client, guest_host, guest_port), daemon=True).start()


if __name__ == "__main__":
    raise SystemExit(main())
