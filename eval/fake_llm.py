"""A fake LLM endpoint that replays a fixed list of turns, one per request.

    python3 fake_llm.py TURNS.json [--port N] [--log FILE]

TURNS.json is a list of turns; the Nth LLM request gets the Nth turn, whatever it asks:

    [{"text": "hi", "usage": {"prompt_tokens": 100, "completion_tokens": 20}},
     {"tool_calls": [{"name": "write", "arguments": {"path": "a", "content": "b"}}]}]

A turn may also carry `"delay": SECONDS`, slept before replying, for tests that must see
something else (a control socket line) land first; replays are otherwise instant.

Both genji wire formats are served (`/chat/completions`, `/messages`), plus the model
context probes (`/models`, `/props`). A request past the last turn gets HTTP 500, so a
script that no longer matches what genji does fails instead of passing by accident.
Prints the bound port on stdout.
"""

import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CONTEXT_WINDOW = 128000
USAGE = {"prompt_tokens": 100, "completion_tokens": 20, "cached_tokens": 0}


def openai_reply(turn, n):
    usage = {**USAGE, **turn.get("usage", {})}
    calls = [
        {
            "id": f"call_{n}_{i}",
            "type": "function",
            "function": {"name": c["name"], "arguments": json.dumps(c.get("arguments", {}))},
        }
        for i, c in enumerate(turn.get("tool_calls", []))
    ]
    message = {"role": "assistant", "content": turn.get("text")}
    if calls:
        message["tool_calls"] = calls
    return {
        "id": f"fake-{n}",
        "object": "chat.completion",
        "model": "fake",
        "choices": [
            {"index": 0, "message": message, "finish_reason": "tool_calls" if calls else "stop"}
        ],
        "usage": {
            "prompt_tokens": usage["prompt_tokens"],
            "completion_tokens": usage["completion_tokens"],
            "total_tokens": usage["prompt_tokens"] + usage["completion_tokens"],
            "prompt_tokens_details": {"cached_tokens": usage["cached_tokens"]},
        },
    }


def anthropic_reply(turn, n):
    usage = {**USAGE, **turn.get("usage", {})}
    blocks = []
    if turn.get("text"):
        blocks.append({"type": "text", "text": turn["text"]})
    for i, c in enumerate(turn.get("tool_calls", [])):
        blocks.append(
            {
                "type": "tool_use",
                "id": f"toolu_{n}_{i}",
                "name": c["name"],
                "input": c.get("arguments", {}),
            }
        )
    return {
        "id": f"fake-{n}",
        "type": "message",
        "role": "assistant",
        "model": "fake",
        "content": blocks,
        "stop_reason": "tool_use" if turn.get("tool_calls") else "end_turn",
        "usage": {
            "input_tokens": usage["prompt_tokens"] - usage["cached_tokens"],
            "output_tokens": usage["completion_tokens"],
            "cache_read_input_tokens": usage["cached_tokens"],
        },
    }


class FakeLlm(ThreadingHTTPServer):
    """Replays `turns`; `requests` collects the request bodies it saw."""

    daemon_threads = True

    def __init__(self, turns, port=0, log=None):
        super().__init__(("127.0.0.1", port), Handler)
        self.turns = turns
        self.log = log
        self.requests = []
        self.next = 0
        self.lock = threading.Lock()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}/v1"

    def take(self, body):
        """The next turn and its index, or None once the script is exhausted."""
        with self.lock:
            self.requests.append(body)
            if self.log:
                with open(self.log, "a") as f:
                    f.write(json.dumps(body) + "\n")
            if self.next >= len(self.turns):
                return None, self.next
            self.next += 1
            return self.turns[self.next - 1], self.next - 1

    def __enter__(self):
        threading.Thread(target=self.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.shutdown()
        self.server_close()


class Handler(BaseHTTPRequestHandler):
    def send(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0].rstrip("/")
        if path.endswith("/props"):
            self.send(200, {"default_generation_settings": {"n_ctx": CONTEXT_WINDOW}})
        elif "/models/" in path:
            self.send(200, {"id": path.rsplit("/", 1)[1], "max_input_tokens": CONTEXT_WINDOW})
        elif path.endswith("/models"):
            self.send(200, {"data": []})
        else:
            self.send(404, {"error": f"fake llm: no route {path}"})

    def do_POST(self):
        path = self.path.split("?")[0].rstrip("/")
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if path.endswith("/chat/completions"):
            render = openai_reply
        elif path.endswith("/messages"):
            render = anthropic_reply
        else:
            return self.send(404, {"error": f"fake llm: no route {path}"})
        turn, n = self.server.take(body)
        if turn is None:
            return self.send(500, {"error": f"fake llm: no turn {n + 1}, the script has {n}"})
        time.sleep(turn.get("delay", 0))
        self.send(200, render(turn, n))

    def log_message(self, *args):
        pass


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("turns", help="JSON file: the list of turns to replay")
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--log", help="append each request body to this JSONL file")
    args = ap.parse_args()
    with open(args.turns) as f:
        turns = json.load(f)
    server = FakeLlm(turns, args.port, args.log)
    print(server.server_address[1], flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
