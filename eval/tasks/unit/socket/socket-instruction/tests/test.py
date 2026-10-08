"""The control socket answers /ping and queues a plain line as a user instruction."""
import json
from pathlib import Path

events = [json.loads(l) for l in Path("/genji/trace/all.jsonl").read_text().splitlines()]
replies = [l for l in Path("/genji/trace/socket.log").read_text().splitlines() if l.startswith("< ")]
assert any("pong" in r for r in replies), f"no pong among the socket replies {replies}"
assert any(e["type"] == "user" and "b.txt" in e["content"] for e in events), "the instruction never arrived"
assert Path("/app/b.txt").read_text().removesuffix("\n") == "b", "wrong b.txt"
