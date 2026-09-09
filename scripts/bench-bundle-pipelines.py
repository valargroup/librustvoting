#!/usr/bin/env python3
"""Run isolated benchmark samples and report OS resource use alongside SDK timing."""
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import time

root = Path(__file__).resolve().parent.parent
# Each resource wrapper runs in its own process so peak RSS is per sample.
if len(sys.argv) > 1 and sys.argv[1] == "sample":
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    subprocess.run(["make", "bench-bundle-pipelines-sample"], cwd=root, check=True)
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    elapsed = time.monotonic() - started
    cpu = usage.ru_utime + usage.ru_stime - before.ru_utime - before.ru_stime
    print(json.dumps({"process_wall_seconds": elapsed, "process_cpu_seconds": cpu,
                      "cpu_percent": 100 * cpu / elapsed,
                      "peak_rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
                      "concurrency": int(os.environ["BUNDLE_BENCH_CONCURRENCY"]),
                      "cache": os.environ["BUNDLE_BENCH_CACHE"]}), flush=True)
else:
    # Compile once before measured samples; no benchmark is run without the opt-in variable.
    subprocess.run(["make", "bench-bundle-pipelines-sample"], cwd=root, check=True,
                   env={key: value for key, value in os.environ.items() if key != "BUNDLE_BENCH_CONCURRENCY"})
    for cache in ("cold", "warm"):
        for concurrency in (1, 5):
            env = dict(os.environ, BUNDLE_BENCH_CONCURRENCY=str(concurrency), BUNDLE_BENCH_CACHE=cache)
            subprocess.run([sys.executable, __file__, "sample"], cwd=root, env=env, check=True)
