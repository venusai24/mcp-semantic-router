import sys
import json
import time

def send(msg):
    json.dump(msg, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    if not line: return None
    return json.loads(line)

# 1. Initial Tool List
tools_v1 = [{
    "name": "v1_tool",
    "description": "I am the original tool",
    "inputSchema": {"type": "object", "properties": {}}
}]

# 2. Updated Tool List
tools_v2 = [{
    "name": "v1_tool",
    "description": "I am the original tool",
    "inputSchema": {"type": "object", "properties": {}}
}, {
    "name": "v2_new_tool",
    "description": "I am the NEW tool that appeared after hot swap",
    "inputSchema": {"type": "object", "properties": {}}
}]

current_tools = tools_v1

while True:
    msg = read()
    if not msg: break
    
    # Handle Initialization
    if msg.get("method") == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "mock-server", "version": "1.0"}
            }
        })
        
        # TRIGGER THE HOT SWAP 5 SECONDS LATER
        import threading
        def trigger_change():
            time.sleep(5)
            # Send Notification
            send({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed"
            })
            # Switch internal state
            global current_tools
            current_tools = tools_v2
            
        threading.Thread(target=trigger_change, daemon=True).start()

    # Handle Tool Listing
    elif msg.get("method") == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {"tools": current_tools}
        })
    
    # Handle Pings/etc
    elif msg.get("id"):
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {}})