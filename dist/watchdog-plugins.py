#!/usr/bin/env python3
# watchdog-plugins.py - Deployment-side watchdog for external plugin processes
# (Linux package; UV-182 legacy L1). Semantics mirror watchdog-plugins.ps1
# (Windows) so both deployment sides behave identically:
#   - acts only on plugins reported "offline" by GET /api/health
#   - "unauthorized" (401/403) is surfaced loudly but NEVER acted on: the
#     process is alive, restarting does not help
#   - no_probe / absent plugins are reported as-is and never acted on
#   - server health fetch failures tolerate health_fail_threshold consecutive
#     bad cycles (default 3) before skipping action cycles; every failure logs
#     WARN, threshold-and-beyond logs ERROR
#   - debounce: restart only after offline_threshold consecutive offline cycles
#   - restart budget: max_restarts_per_hour per plugin; BOTH successful and
#     FAILED start attempts consume budget so a misconfigured command converges
#     to the escalation latch instead of retrying forever
#   - over budget the watchdog stops attempting and emits an ESCALATION log
#   - the escalation latch clears automatically once the plugin is seen online
#   - data/watchdog.log rotates at 5 MB into watchdog.log.old (single gen)
#   - the audit-facing trail stays in the server platform events
# Requires: python3 (stdlib only). ASCII-only log output on purpose.
import json
import os
import signal
import subprocess
import sys
import time
import urllib.request

BASE = os.path.dirname(os.path.abspath(__file__))
LOG_PATH = os.path.join(BASE, "data", "watchdog.log")
PID_PATH = os.path.join(BASE, "data", "watchdog.pid")
LOG_MAX_BYTES = 5 * 1024 * 1024


def log(level, msg):
    line = "%s [watchdog] %s %s" % (time.strftime("%Y-%m-%d %H:%M:%S"), level, msg)
    print(line, flush=True)
    try:
        os.makedirs(os.path.dirname(LOG_PATH), exist_ok=True)
        if os.path.exists(LOG_PATH) and os.path.getsize(LOG_PATH) >= LOG_MAX_BYTES:
            os.replace(LOG_PATH, LOG_PATH + ".old")
        with open(LOG_PATH, "a", encoding="ascii", errors="replace") as f:
            f.write(line + "\n")
    except OSError:
        pass


def main():
    # single-instance guard via pid file (best effort, mirrors the PS1 mutex)
    os.makedirs(os.path.dirname(PID_PATH), exist_ok=True)
    if os.path.exists(PID_PATH):
        try:
            old = int(open(PID_PATH).read().strip())
            os.kill(old, 0)
            log("WARN", "another watchdog instance appears running (pid %d); exit" % old)
            return 0
        except (ValueError, ProcessLookupError, PermissionError, OSError):
            pass  # stale pid file: take over
    with open(PID_PATH, "w") as f:
        f.write(str(os.getpid()))

    def stop(signum, frame):
        try:
            os.remove(PID_PATH)
        except OSError:
            pass
        sys.exit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    cfg_path = os.path.join(BASE, "plugins-watchdog.json")
    if not os.path.exists(cfg_path):
        log("ERROR", "config not found: %s" % cfg_path)
        return 1
    try:
        with open(cfg_path, encoding="utf-8") as f:
            cfg = json.load(f)
    except ValueError as e:
        log("ERROR", "config parse failed: %s" % e)
        return 1

    plugins = cfg.get("plugins") or {}
    if not plugins:
        log("INFO", "no plugins configured in plugins-watchdog.json; nothing to watch; exit")
        return 0

    interval = int(cfg.get("interval_secs") or 10)
    threshold = int(cfg.get("offline_threshold") or 3)
    max_restarts = int(cfg.get("max_restarts_per_hour") or 5)
    fetch_timeout = int(cfg.get("fetch_timeout_secs") or 5)
    health_url = cfg.get("health_url") or "http://127.0.0.1:18080/api/health"
    fail_tolerance = int(cfg.get("health_fail_threshold") or 3)

    log("INFO", "watchdog start: url=%s interval=%ds threshold=%d max_restarts/h=%d fail_tolerance=%d plugins=[%s]"
        % (health_url, interval, threshold, max_restarts, fail_tolerance, ",".join(sorted(plugins))))

    offline_count = {k: 0 for k in plugins}
    restarts = {k: [] for k in plugins}
    latched = {k: False for k in plugins}
    health_fails = 0

    while True:
        status_map = None
        try:
            with urllib.request.urlopen(health_url, timeout=fetch_timeout) as r:
                body = json.loads(r.read().decode("utf-8", "replace"))
            status_map = {}
            for name, info in (body.get("plugins") or {}).items():
                status_map[name] = (info or {}).get("status")
        except Exception:
            status_map = None

        if not status_map:
            health_fails += 1
            if health_fails >= fail_tolerance:
                log("ERROR", "server health bad %d/%d (unreachable or empty); skip cycle (no action); HUMAN ATTENTION if persistent"
                    % (health_fails, fail_tolerance))
            else:
                log("WARN", "server health bad %d/%d (unreachable or empty); skip cycle (no action)"
                    % (health_fails, fail_tolerance))
        else:
            if health_fails > 0:
                log("INFO", "server health recovered after %d bad cycle(s); fail counter reset" % health_fails)
            health_fails = 0
            now = time.time()
            for pid_name in sorted(plugins):
                conf = plugins[pid_name] or {}
                status = status_map.get(pid_name)
                if status is None:
                    if offline_count[pid_name] != 0:
                        log("INFO", "%s: status no_probe/absent; no action (as-is)" % pid_name)
                    offline_count[pid_name] = 0
                    continue
                if status == "online":
                    if offline_count[pid_name] > 0:
                        log("INFO", "%s: back online; counter reset" % pid_name)
                    if latched[pid_name]:
                        latched[pid_name] = False
                        log("INFO", "%s: escalation latch cleared (online observed)" % pid_name)
                    offline_count[pid_name] = 0
                    continue
                if status == "unauthorized":
                    if offline_count[pid_name] != 0:
                        log("WARN", "%s: status unauthorized (credentials/config problem); no action (restart would not help); check token env config" % pid_name)
                    offline_count[pid_name] = 0
                    continue
                if status != "offline":
                    continue  # unknown future states: no action
                offline_count[pid_name] += 1
                if offline_count[pid_name] < threshold:
                    log("INFO", "%s: offline %d/%d" % (pid_name, offline_count[pid_name], threshold))
                    continue
                if latched[pid_name]:
                    log("WARN", "%s: offline and escalation-latched; NOT restarting (budget exhausted); human action required" % pid_name)
                    continue
                restarts[pid_name] = [t for t in restarts[pid_name] if t > now - 3600]
                if len(restarts[pid_name]) >= max_restarts:
                    latched[pid_name] = True
                    log("ESCALATION", "%s: restart budget exhausted (%d/hour); watchdog gives up; HUMAN ACTION REQUIRED"
                        % (pid_name, len(restarts[pid_name])))
                    continue
                cmd = conf.get("command")
                if not cmd:
                    log("ERROR", "%s: config missing 'command'; cannot start" % pid_name)
                    continue
                full = cmd if os.path.exists(cmd) else os.path.join(BASE, cmd)
                if not os.path.exists(full):
                    log("ERROR", "%s: command not found: %s" % (pid_name, cmd))
                    continue
                wd = os.path.join(BASE, conf["working_dir"]) if conf.get("working_dir") else BASE
                env = dict(os.environ)
                env.update(conf.get("env") or {})
                args = [full] + list(conf.get("args") or [])
                log("WARN", "%s: offline %d/%d; attempting restart #%d"
                    % (pid_name, offline_count[pid_name], threshold, len(restarts[pid_name]) + 1))
                try:
                    subprocess.Popen(args, cwd=wd, env=env, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
                    restarts[pid_name].append(now)
                    offline_count[pid_name] = 0
                    log("INFO", "%s: process started (%s)" % (pid_name, " ".join(args)))
                except OSError as e:
                    # failed start also consumes the restart budget (batch D parity)
                    restarts[pid_name].append(now)
                    log("WARN", "%s: start attempt FAILED (%s); consumed restart budget (%d/hour); latch re-evaluated next cycle"
                        % (pid_name, e, len(restarts[pid_name])))
        time.sleep(interval)


if __name__ == "__main__":
    sys.exit(main())
