import subprocess
import json
import time
import sys
import threading

ROUTER_BIN = "./target/release/mcp-router"

def read_stream(process):
    """Continuously read lines from router stdout"""
    while True:
        line = process.stdout.readline()
        if not line: break
        try:
            msg = json.loads(line.decode('utf-8'))
            if "result" in msg:
                # Identify which request finished
                if msg.get("id") == "status_check":
                    print(f"✅ Router Status returned! Content: {msg['result']['content'][0]['text']}")
                elif msg.get("id") == "git_check":
                    print(f"✅ Git Status returned!")
        except:
            pass

def run_test():
    print("🚀 Starting Router...")
    process = subprocess.Popen([ROUTER_BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr)
    
    # Start a background thread to read responses
    threading.Thread(target=read_stream, args=(process,), daemon=True).start()
    time.sleep(3) # Wait for docker boot

    # Request 1: Git Status (The "Slow" one)
    print("📨 Sending Request 1: Git Status")
    req1 = json.dumps({
        "jsonrpc": "2.0", "id": "git_check", "method": "tools/call",
        "params": {"name": "git___git_status", "arguments": {"repo_path": "/projects"}}
    }) + "\n"
    process.stdin.write(req1.encode())
    process.stdin.flush()

    # Request 2: Router Status (The "Fast" one) - Sent IMMEDIATELY after
    print("📨 Sending Request 2: Router Status (Immediately)")
    req2 = json.dumps({
        "jsonrpc": "2.0", "id": "status_check", "method": "tools/call",
        "params": {"name": "router_status"}
    }) + "\n"
    process.stdin.write(req2.encode())
    process.stdin.flush()

    time.sleep(2)
    process.terminate()

if __name__ == "__main__":
    run_test()