#!/usr/bin/env python3
"""Dispatch and wait for the production workflow in the infrastructure repo."""

import json
import os
from pathlib import Path
import subprocess
import time

REPO = "nearai/cvm-ansible-playbooks"
WORKFLOW = "update_cloud_api_prod.yml"


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True, timeout=60)


def deploy():
    correlation = f"{os.environ['GITHUB_REPOSITORY']}/{os.environ['GITHUB_RUN_ID']}/{os.environ['GITHUB_RUN_ATTEMPT']}"
    title = f"Update Cloud API Prod [{correlation}]"
    gh("workflow", "run", WORKFLOW, "--repo", REPO, "--ref", "main",
       "-f", f"release_id={correlation}")
    deadline = time.monotonic() + 7200
    discovery_deadline = time.monotonic() + 300
    run_id = None
    while time.monotonic() < deadline:
        if run_id is None:
            runs = json.loads(gh("run", "list", "--repo", REPO, "--workflow", WORKFLOW,
                                 "--event", "workflow_dispatch", "--branch", "main",
                                 "--limit", "100", "--json", "databaseId,displayTitle"))
            matches = [run for run in runs if run["displayTitle"] == title]
            if len(matches) > 1:
                raise RuntimeError(f"Multiple deployment runs match {correlation}")
            if matches:
                run_id = str(matches[0]["databaseId"])
                url = f"https://github.com/{REPO}/actions/runs/{run_id}"
                print(f"Deployment: {url}", flush=True)
                with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
                    summary.write(f"\nProduction deployment: [run {run_id}]({url})\n")
            elif time.monotonic() >= discovery_deadline:
                raise TimeoutError("Could not find dispatched deployment within five minutes")
        if run_id is not None:
            run = json.loads(gh("run", "view", run_id, "--repo", REPO,
                                "--json", "status,conclusion"))
            if run["status"] == "completed":
                if run["conclusion"] != "success":
                    raise RuntimeError(f"Production deployment concluded: {run['conclusion']}")
                print("Production deployment and health checks succeeded.")
                return
        time.sleep(15)
    raise TimeoutError("Production deployment did not finish within two hours; inspect the linked run before retrying")


if __name__ == "__main__":
    try:
        deploy()
    except (RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
        print(f"::error::{error}", flush=True)
        raise SystemExit(1)
