#!/usr/bin/env python3

import argparse
import asyncio
import os
import ssl
import sys
import traceback

from aioquic.asyncio.client import connect
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.events import ConnectionTerminated
from aioquic.quic.configuration import QuicConfiguration


class H3InteropClient(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._http = H3Connection(self._quic)
        self._pending = {}

    async def get(self, authority: str, path: str):
        stream_id = self._quic.get_next_available_stream_id()
        waiter = self._loop.create_future()
        self._pending[stream_id] = {
            "headers": [],
            "body": bytearray(),
            "waiter": waiter,
        }

        self._http.send_headers(
            stream_id,
            [
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":authority", authority.encode("utf-8")),
                (b":path", path.encode("utf-8")),
                (b"user-agent", b"starfish-aioquic-interop"),
            ],
            end_stream=True,
        )
        self.transmit()
        return await asyncio.wait_for(waiter, timeout=10.0)

    def quic_event_received(self, event):
        if os.environ.get("STARFISH_AIOQUIC_TRACE"):
            print(f"aioquic event: {type(event).__name__}", file=sys.stderr)
        if isinstance(event, ConnectionTerminated):
            if os.environ.get("STARFISH_AIOQUIC_TRACE"):
                print(
                    f"aioquic terminated: error_code={event.error_code} reason={event.reason_phrase!r}",
                    file=sys.stderr,
                )
            message = f"connection terminated: {event.error_code} {event.reason_phrase}"
            for state in self._pending.values():
                waiter = state["waiter"]
                if not waiter.done():
                    waiter.set_exception(RuntimeError(message))
            return

        for http_event in self._http.handle_event(event):
            state = self._pending.get(http_event.stream_id)
            if state is None:
                continue

            if isinstance(http_event, HeadersReceived):
                state["headers"].extend(http_event.headers)
                if http_event.stream_ended:
                    self._finish(http_event.stream_id)
            elif isinstance(http_event, DataReceived):
                state["body"].extend(http_event.data)
                if http_event.stream_ended:
                    self._finish(http_event.stream_id)

    def _finish(self, stream_id: int):
        state = self._pending.pop(stream_id, None)
        if state is None:
            return

        waiter = state["waiter"]
        if not waiter.done():
            waiter.set_result(
                {
                    "headers": state["headers"],
                    "body": bytes(state["body"]),
                }
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--server-name", required=True)
    parser.add_argument("--authority", required=True)
    parser.add_argument("--path", required=True)
    parser.add_argument("--ca-cert", required=True)
    return parser.parse_args()


def decode_headers(header_pairs):
    decoded = []
    for name, value in header_pairs:
        decoded.append(
            (
                name.decode("utf-8", errors="replace"),
                value.decode("utf-8", errors="replace"),
            )
        )
    return decoded


async def run(args: argparse.Namespace):
    configuration = QuicConfiguration(
        is_client=True,
        alpn_protocols=["h3"],
        server_name=args.server_name,
        verify_mode=ssl.CERT_REQUIRED,
    )
    configuration.load_verify_locations(cafile=args.ca_cert)

    async with connect(
        args.host,
        args.port,
        configuration=configuration,
        create_protocol=H3InteropClient,
        wait_connected=False,
    ) as client:
        client.transmit()
        await asyncio.wait_for(client.wait_connected(), timeout=10.0)
        response = await client.get(args.authority, args.path)

    headers = decode_headers(response["headers"])
    status = next((value for name, value in headers if name == ":status"), None)
    body = response["body"].decode("utf-8", errors="replace")

    print(f"status: {status}")
    print(f"body: {body}")


def main() -> int:
    args = parse_args()
    try:
        asyncio.run(run(args))
    except Exception as exc:
        traceback.print_exc()
        print(f"error: {exc!r}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
