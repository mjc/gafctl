"""Exercise a source installation without connecting to devices."""

import argparse
import json
import os
import socket
import subprocess
import time
from pathlib import Path
from urllib.request import urlopen

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--package", type=Path, help="Check an existing installation")
package = parser.parse_args().package
installation = root / "target/install-native"
installation.mkdir(parents=True, exist_ok=True)
if package is None:
    subprocess.run(
        [
            "cargo",
            "install",
            "--path",
            str(root),
            "--locked",
            "--root",
            str(installation),
        ],
        check=True,
    )
package = package or installation
for executable in ("gafctl", "gafctl-server"):
    subprocess.run(
        [str(package / "bin" / executable), "--help"],
        check=True,
        stdout=subprocess.DEVNULL,
    )
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
environment = {
    key: value for key, value in os.environ.items() if not key.startswith("GAFCTL_")
}
environment["GAFCTL_IDENTITY_STORE"] = str(installation / "identities.json")
with subprocess.Popen(
    [str(package / "bin/gafctl"), "server", "--bind", f"127.0.0.1:{port}"],
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
        returncode = server.wait(timeout=5)
        assert returncode == 0, f"installed server exited with status {returncode}"
print("Installed binaries, sibling server launch, HTTP, graceful shutdown: passed")
