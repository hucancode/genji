"""Runs the genji command in argv and, after its 1st tool call, sends `/stop` to its control
socket, logging the line and reply to /genji/trace/socket.log."""
import json
import socket
import subprocess
import sys


def ask(path, line):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(10)
        s.connect(path)
        s.sendall(f"{line}\n".encode())
        return s.makefile().readline().rstrip("\n")


genji = subprocess.Popen(sys.argv[1:], stdout=subprocess.PIPE, text=True)
sock, sent = None, False
with open("/genji/trace/socket.log", "a") as log:
    for line in genji.stdout:
        sys.stdout.write(line)
        sys.stdout.flush()
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("type") == "instance_start":
            sock = event.get("control_socket")
        elif event.get("type") == "tool_call" and sock and not sent:
            sent = True
            try:
                reply = ask(sock, "/stop")
            except OSError as e:
                reply = f"wrap: {e}"
            log.write(f"> /stop\n< {reply}\n")
sys.exit(genji.wait())
