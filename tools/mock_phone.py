#!/usr/bin/env python3
"""Mock opencamo phone for smoke-testing the desktop client.

Serves /v1/capabilities, /v1/ping, and /v1/stream (multipart MJPEG) exactly
per PROTOCOL.md, generating synthetic JPEG frames with a frame counter so
decode progress is visible.

Usage: python3 tools/mock_phone.py [--port 47777] [--width 640] [--height 480] [--fps 30]
"""
import argparse
import io
import json
import socket
import struct
import threading
import time

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    from PIL import Image, ImageDraw
except ImportError:
    Image = None


BOUNDARY = "frame"
_state = {
    "frames": 0,
    "clients": 0,
    "lock": threading.Lock(),
    "frame_bytes": None,   # latest JPEG, updated by generator
    "frame_event": threading.Event(),
}


def make_frame(width, height, n):
    """Synthetic JPEG frame: moving bar + frame counter, or plain gray if no PIL."""
    if Image is None:
        # Minimal valid JPEG: 1x1 pixel (fallback so streaming logic still testable)
        raise SystemExit("Pillow required: pip install pillow")
    img = Image.new("RGB", (width, height), (30, 30, 40))
    d = ImageDraw.Draw(img)
    x = (n * 8) % width
    d.rectangle([x, 0, min(x + 60, width), height], fill=(0, 200, 120))
    d.text((20, 20), f"opencamo mock  frame={n}", fill=(255, 255, 255))
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=80)
    return buf.getvalue()


def generator(width, height, fps):
    n = 0
    interval = 1.0 / fps
    while True:
        jpeg = make_frame(width, height, n)
        with _state["lock"]:
            _state["frame_bytes"] = jpeg
            _state["frames"] = n
        _state["frame_event"].set()
        n += 1
        time.sleep(interval)


class Handler(BaseHTTPRequestHandler):
    server_version = "opencamo-mock/0.1"

    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        path, _, query = self.path.partition("?")
        params = dict(p.split("=", 1) for p in query.split("&") if "=" in p)
        if path == "/v1/ping":
            self._reply(200, b"pong", "text/plain")
        elif path == "/v1/capabilities":
            caps = {
                "protocol": 1,
                "app": "opencamo-mock",
                "version": "0.1.0",
                "device_name": "Mock Phone",
                "cameras": [
                    {
                        "id": "back",
                        "label": "Back camera",
                        "resolutions": [
                            {"width": self.server.width, "height": self.server.height,
                             "fps": [self.server.fps]},
                        ],
                    }
                ],
            }
            self._reply(200, json.dumps(caps).encode(), "application/json")
        elif path == "/v1/stream":
            self._stream(params)
        else:
            self._reply(404, json.dumps({"error": "not found"}).encode(),
                        "application/json")

    def _reply(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _stream(self, params):
        width = int(params.get("width", self.server.width))
        height = int(params.get("height", self.server.height))
        if (width, height) != (self.server.width, self.server.height):
            self._reply(400, json.dumps({"error": "unsupported resolution"}).encode(),
                        "application/json")
            return
        self.send_response(200)
        self.send_header("Content-Type",
                         f"multipart/x-mixed-replace; boundary={BOUNDARY}")
        self.end_headers()
        with _state["lock"]:
            _state["clients"] += 1
        try:
            last_sent = -1
            while True:
                _state["frame_event"].wait(timeout=5)
                _state["frame_event"].clear()
                with _state["lock"]:
                    n = _state["frames"]
                    jpeg = _state["frame_bytes"]
                if jpeg is None or n == last_sent:
                    continue
                last_sent = n
                part = (f"--{BOUNDARY}\r\n"
                        f"Content-Type: image/jpeg\r\n"
                        f"Content-Length: {len(jpeg)}\r\n\r\n").encode() + jpeg + b"\r\n"
                self.wfile.write(part)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with _state["lock"]:
                _state["clients"] -= 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=47777)
    ap.add_argument("--width", type=int, default=640)
    ap.add_argument("--height", type=int, default=480)
    ap.add_argument("--fps", type=int, default=30)
    args = ap.parse_args()

    server = ThreadingHTTPServer(("0.0.0.0", args.port), Handler)
    server.width, server.height, server.fps = args.width, args.height, args.fps
    threading.Thread(target=generator, args=(args.width, args.height, args.fps),
                     daemon=True).start()
    print(f"mock phone on :{args.port}  {args.width}x{args.height}@{args.fps}")
    server.serve_forever()


if __name__ == "__main__":
    main()
