#!/usr/bin/env python3
"""Manual sudo_run smoke test. Runs `sudo id` through the MCP server; the
password is entered in the OS dialog, never seen by this script. Run it from a
desktop session (needs $DISPLAY / Wayland for zenity/kdialog)."""
import json, subprocess, sys

BIN = "./target/release/pty-mcp"
p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL, text=True, bufsize=1)

def send(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
def recv(): return json.loads(p.stdout.readline())

send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},
    "clientInfo":{"name":"sudo-smoke","version":"0"}}})
recv()
send({"jsonrpc":"2.0","method":"notifications/initialized"})

argv = sys.argv[1:] or ["id"]
print(f"Running: sudo {' '.join(argv)}  (dialog will pop — enter your password)")
send({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
    "name":"sudo_run","arguments":{
        "argv": argv, "reason":"manual smoke test", "timeout_seconds":120}}})
r = recv()
out = json.loads(r["result"]["content"][0]["text"])
print(json.dumps(out, indent=2))
p.stdin.close(); p.terminate()
