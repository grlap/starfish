#!/usr/bin/env python3

import argparse
import asyncio
import os
import sys
import traceback

from aioquic.asyncio import serve
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import ConnectionTerminated


class H3InteropServerProtocol(QuicConnectionProtocol):
    def __init__(self, *args, response_body: bytes, request_done, **kwargs):
        super().__init__(*args, **kwargs)
        self._http = H3Connection(self._quic)
        self._response_body = response_body
        self._request_done = request_done
        self._pending = {}

    def quic_event_received(self, event):
        if os.environ.get("STARFISH_AIOQUIC_TRACE"):
            print(f"aioquic event: {type(event).__name__}", file=sys.stderr)

        if isinstance(event, ConnectionTerminated):
            if not self._request_done.done():
                self._request_done.set_exception(
                    RuntimeError(
                        f"connection terminated: {event.error_code} {event.reason_phrase}"
                    )
                )
            return

        for http_event in self._http.handle_event(event):
            state = self._pending.setdefault(
                http_event.stream_id,
                {
                    "headers": [],
                    "body": bytearray(),
                },
            )

            if isinstance(http_event, HeadersReceived):
                state["headers"].extend(http_event.headers)
                if http_event.stream_ended:
                    self._respond(http_event.stream_id)
            elif isinstance(http_event, DataReceived):
                state["body"].extend(http_event.data)
                if http_event.stream_ended:
                    self._respond(http_event.stream_id)

    def _respond(self, stream_id: int):
        state = self._pending.pop(stream_id, None)
        if state is None:
            return

        self._http.send_headers(
            stream_id,
            [
                (b":status", b"200"),
                (b"content-type", b"text/plain"),
            ],
            end_stream=not self._response_body,
        )
        if self._response_body:
            self._http.send_data(stream_id, self._response_body, end_stream=True)
        self.transmit()

        if not self._request_done.done():
            self._request_done.set_result(
                {
                    "headers": decode_headers(state["headers"]),
                    "body": bytes(state["body"]).decode("utf-8", errors="replace"),
                }
            )


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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--cert", required=True)
    parser.add_argument("--key", required=True)
    parser.add_argument("--ready-file", required=True)
    parser.add_argument("--response-body", default="aioquic-ok")
    return parser.parse_args()


async def run(args: argparse.Namespace):
    configuration = QuicConfiguration(
        is_client=False,
        alpn_protocols=["h3"],
    )
    configuration.load_cert_chain(args.cert, args.key)

    loop = asyncio.get_running_loop()
    request_done = loop.create_future()
    server = await serve(
        args.host,
        args.port,
        configuration=configuration,
        create_protocol=lambda quic, stream_handler=None: H3InteropServerProtocol(
            quic,
            stream_handler=stream_handler,
            response_body=args.response_body.encode("utf-8"),
            request_done=request_done,
        ),
    )

    with open(args.ready_file, "w", encoding="utf-8") as ready_file:
        ready_file.write("ready\n")

    try:
        request = await asyncio.wait_for(request_done, timeout=10.0)
    finally:
        server.close()
        await asyncio.sleep(0.05)

    headers = request["headers"]
    method = next((value for name, value in headers if name == ":method"), "")
    path = next((value for name, value in headers if name == ":path"), "")
    user_agent = next((value for name, value in headers if name == "user-agent"), "")

    print(f"method: {method}")
    print(f"path: {path}")
    print(f"user-agent: {user_agent}")
    print(f"body: {request['body']}")


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
