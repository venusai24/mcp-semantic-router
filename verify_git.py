import subprocess
import json
import time
import sys

# Path to your compiled router binary
ROUTER_BIN = "./target/release/mcp-router"

def run_test():
    print(f"🚀 Launching Router: {ROUTER_BIN}")
    
    # Start the router process
    process = subprocess.Popen(
        [ROUTER_BIN],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr  # Let router logs show in terminal
    )

    # 1. WAIT for Docker to spin up (Critical!)
    # The 'git' server inside Docker takes a few seconds to boot.
    print("⏳ Waiting 3 seconds for Docker Git server to initialize...")
    time.sleep(3)

    # 2. Prepare the Request JSON
    # We ask the router to call 'git_status' on the 'git' server
    request = {
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "git___git_status",
            "arguments": {
                "repo_path": "/projects" 
            }
        }
    }

    print(f"\n📨 Sending Request: {json.dumps(request)}")
    
    # Write to Router's STDIN
    json_str = json.dumps(request) + "\n"
    process.stdin.write(json_str.encode('utf-8'))
    process.stdin.flush()

    # 3. Read the Response
    print("Inbox: Waiting for response...")
    while True:
        line = process.stdout.readline()
        if not line:
            break
            
        try:
            response = json.loads(line.decode('utf-8'))
            
            # Check if this is the response to our ID 5
            if response.get("id") == 5:
                print("\n🎉 RESPONSE RECEIVED:")
                print(json.dumps(response, indent=2))
                
                # Validation Logic
                if "result" in response and "content" in response["result"]:
                    content_text = response["result"]["content"][0]["text"]
                    print("\n✅ VERIFICATION PASSED: Router successfully executed git command!")
                    print(f"📄 Git Output Snippet:\n{content_text[:200]}...")
                    break
                elif "error" in response:
                    print(f"\n❌ VERIFICATION FAILED: Router returned an error.")
                    print(f"Error Message: {response['error']['message']}")
                    break
                    
        except json.JSONDecodeError:
            continue

    # Cleanup
    process.terminate()

if __name__ == "__main__":
    run_test()