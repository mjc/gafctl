"""GitHub CLI fixture used by the release script tests."""

import json
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
state_file = Path(os.environ["GITHUB_STUB_STATE"])
state = json.loads(state_file.read_text())
with Path(os.environ["GITHUB_STUB_LOG"]).open("a") as log:
    log.write(json.dumps(args) + "\n")

if args[0] == "api":
    if error := state.get("api_error"):
        sys.exit(error)
    endpoint = next(arg for arg in args if arg.startswith("repos/"))
    if "/git/tags/" in endpoint:
        response = state["tag"]
    elif "/commits/" in endpoint:
        response = state["commit"]
    elif endpoint.endswith("/releases?per_page=100"):
        response = [state["releases"]]
    elif endpoint.endswith("/assets?per_page=100"):
        response = [[{"name": name} for name in state["assets"]]]
    elif endpoint.endswith("/releases/1"):
        response = state["releases"][0]
    else:
        sys.exit(f"Unexpected API endpoint: {endpoint}")
    print(json.dumps(response))
elif args[:2] == ["release", "download"]:
    asset = args[args.index("--pattern") + 1]
    directory = Path(args[args.index("--dir") + 1])
    shutil.copyfile(
        Path(os.environ["GITHUB_STUB_DOWNLOADS"]) / asset, directory / asset
    )
elif args[:2] in (["release", "upload"], ["release", "create"]):
    state["assets"] = sorted(
        set(state["assets"])
        | {Path(arg).name for arg in args[3:] if arg.startswith("dist/")}
    )
    if args[1] == "create":
        state["releases"] = [
            {"id": 1, "tag_name": args[2], "draft": False, "prerelease": False}
        ]
    state_file.write_text(json.dumps(state))
elif args[:2] == ["release", "edit"]:
    state["releases"][0].update(draft=False, prerelease=False)
    state_file.write_text(json.dumps(state))
else:
    sys.exit(f"Unexpected gh command: {args}")
