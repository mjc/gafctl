"""Exercise a source installation without connecting to devices."""

import json
import os
import socket
import subprocess
import time
from pathlib import Path
from urllib.request import urlopen

root = Path(__file__).resolve().parents[1]
installation = root / "target/install-native"
subprocess.run(
    ["cargo", "install", "--path", str(root), "--locked", "--root", str(installation)],
    check=True,
)
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
environment = {
    key: value for key, value in os.environ.items() if not key.startswith("GAFCTL_")
}
environment["GAFCTL_IDENTITY_STORE"] = str(installation / "identities.json")
with subprocess.Popen(
    [str(installation / "bin/gafctl"), "server", "--bind", f"127.0.0.1:{port}"],
    env=environment,
) as server:
    try:
        for _attempt in range(30):
            try:
                with urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as response:
                    assert json.load(response) == {"status": "ok"}
                break
            except OSError:
                assert server.poll() is None
                time.sleep(0.1)
        else:
            raise AssertionError("installed server did not become ready")
        with urlopen(f"http://127.0.0.1:{port}/api/v2/devices", timeout=2) as response:
            assert json.load(response) == {"devices": []}
    finally:
        server.terminate()
        server.wait(timeout=5)
print("Source install, sibling server launch, HTTP, graceful shutdown: passed")
