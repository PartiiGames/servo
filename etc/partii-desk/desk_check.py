#!/usr/bin/env python3
"""Partii desk checks: drive a headless servoshell over WebDriver on macOS, Windows or Linux and record, per page:
loads, input (WebDriver key + pointer actions reach the page), saves (localStorage + IndexedDB across a restart),
audio (OfflineAudioContext renders sound; a real-time AudioContext runs), clean exit (closing the last window
ends the process with code 0) and fps (requestAnimationFrame over 3 s). The same checks as the Linux export's
play test (partii-runtime tauri-runtime/tools/lx/playtest.py), ported to servoshell so they run on CI runners.

  python3 desk_check.py --bin <servoshell> --out <dir> [name=url ...]

Pages default to the built-in probe page (served on 127.0.0.1) and hexii (https://hexii.partii.games/, public).
Writes <out>/desk-check.json, <out>/desk-check.md and a screenshot per page. Exit 0 = every page loaded.
Stdlib only."""
import argparse, base64, http.server, json, os, platform, shutil, socket, subprocess, sys, tempfile, threading, time
import urllib.request

PROBE_HTML = r"""<!doctype html><meta charset="utf-8"><title>desk probe</title>
<style>html,body{margin:0;background:#101018;color:#eee;font:16px sans-serif}canvas{display:block}</style>
<canvas id="c" width="640" height="360"></canvas><div id="s">probe</div>
<script>
const c = document.getElementById("c"), gl = c.getContext("webgl2");
let t = 0;
function frame() { t++; if (gl) { gl.clearColor((t % 60) / 60, 0.3, 0.6, 1); gl.clear(gl.COLOR_BUFFER_BIT); } requestAnimationFrame(frame); }
requestAnimationFrame(frame);
window.__probe = async () => {
  const r = {};
  r.webgl2 = !!gl; r.canvas2d = !!document.createElement("canvas").getContext("2d");
  try { const m = await WebAssembly.instantiate(new Uint8Array([0,97,115,109,1,0,0,0,1,5,1,96,0,1,127,3,2,1,0,7,5,1,1,102,0,0,10,6,1,4,0,65,42,11]));
        r.wasm = m.instance.exports.f() === 42; } catch (e) { r.wasm = String(e); }
  try { const w = new Worker(URL.createObjectURL(new Blob(["let n=0;const i=setInterval(()=>{if(++n==30){clearInterval(i);postMessage(n)}},33)"], {type: "text/javascript"})));
        const t0 = performance.now(); await new Promise((res, rej) => { w.onmessage = res; w.onerror = rej; setTimeout(() => rej("timeout"), 5000); });
        r.worker_interval_ms = Math.round((performance.now() - t0) / 30 * 10) / 10; w.terminate(); } catch (e) { r.worker_interval_ms = String(e); }
  r.sharedArrayBuffer = typeof SharedArrayBuffer !== "undefined"; r.gamepad = typeof navigator.getGamepads === "function";
  r.webgpu = !!navigator.gpu;
  return r;
};
</script>"""

# Installed at document start (servoshell --userscripts): counts errors and input events before the game sees them.
USERSCRIPT = r"""(() => { if (window.__desk) return; const d = window.__desk = { errors: [], keys: 0, pointers: 0 };
addEventListener("error", (e) => d.errors.push(String(e.message || e.error || e).slice(0, 300)), true);
addEventListener("unhandledrejection", (e) => d.errors.push("unhandledrejection: " + String(e.reason).slice(0, 300)));
addEventListener("keydown", () => d.keys++, true); addEventListener("pointerdown", () => d.pointers++, true);
addEventListener("mousedown", () => d.pointers++, true); })();"""

FPS_JS = r"""const done = arguments[arguments.length - 1]; let n = 0; const t0 = performance.now();
function f() { n++; if (performance.now() - t0 < 3000) requestAnimationFrame(f); else done(Math.round(n * 1000 / (performance.now() - t0) * 10) / 10); }
requestAnimationFrame(f);"""

AUDIO_JS = r"""const done = arguments[arguments.length - 1]; const r = {};
(async () => {
  try { const o = new OfflineAudioContext(1, 4800, 48000), osc = o.createOscillator(); osc.connect(o.destination); osc.start();
        const b = await o.startRendering(); let peak = 0; for (const v of b.getChannelData(0)) peak = Math.max(peak, Math.abs(v));
        r.offline_peak = Math.round(peak * 1000) / 1000; } catch (e) { r.offline_error = String(e); }
  try { const a = new AudioContext(); await Promise.race([a.resume(), new Promise((s) => setTimeout(s, 2000))]);
        const t0 = a.currentTime; await new Promise((s) => setTimeout(s, 1000));
        r.realtime_state = a.state; r.realtime_advanced_s = Math.round((a.currentTime - t0) * 100) / 100; r.sample_rate = a.sampleRate; a.close(); }
  catch (e) { r.realtime_error = String(e); }
  done(r);
})();"""

SAVE_JS = r"""const done = arguments[arguments.length - 1], token = arguments[0], write = arguments[1]; const r = {};
try { if (write) localStorage.setItem("desk-save", token); r.local = localStorage.getItem("desk-save"); } catch (e) { r.local_error = String(e); }
const req = indexedDB.open("desk-save", 1);
req.onupgradeneeded = () => req.result.createObjectStore("kv");
req.onerror = () => { r.idb_error = String(req.error); done(r); };
req.onsuccess = () => { const db = req.result, tx = db.transaction("kv", "readwrite"), st = tx.objectStore("kv");
  if (write) st.put(token, "k"); const g = st.get("k");
  tx.oncomplete = () => { r.idb = g.result ?? null; db.close(); done(r); }; tx.onerror = () => { r.idb_error = String(tx.error); done(r); }; };"""


def profile_env(profile):
    """servoshell has no --config-dir: its storage lives under HOME (macOS: ~/Library/Application Support/Servo) or
    XDG_CONFIG_HOME (Linux). Windows keeps the runner's own %APPDATA%\\Servo (each page writes a fresh token)."""
    e = dict(os.environ)
    if os.name != "nt": e["HOME"] = profile; e["XDG_CONFIG_HOME"] = os.path.join(profile, ".config")
    return e


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class Shell:
    def __init__(self, binary, profile, usdir, log_path, size="1280x720"):
        self.port = free_port(); self.base = f"http://127.0.0.1:{self.port}"
        args = [binary, "--headless", "--enable-experimental-web-platform-features", f"--webdriver={self.port}",
                f"--window-size={size}", f"--userscripts={usdir}"]
        for p in ("dom_webgl2_enabled", "dom_gamepad_enabled", "dom_indexeddb_enabled", "dom_offscreen_canvas_enabled"):
            args += ["--pref", p]
        args.append("about:blank")
        self.log = open(log_path, "ab")
        self.t0 = time.time()
        self.proc = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT, env=profile_env(profile))
        self.session = None
        for _ in range(120):
            if self.proc.poll() is not None: raise RuntimeError(f"servoshell exited {self.proc.returncode} at start")
            try: self.session = self.call("POST", "/session", {"capabilities": {}}, timeout=5)["sessionId"]; break
            except Exception: time.sleep(0.5)
        if not self.session: raise RuntimeError("no WebDriver session in 60 s")
        self.start_s = round(time.time() - self.t0, 2)

    def call(self, method, path, body=None, timeout=90):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method, headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r: return json.loads(r.read() or b"{}").get("value")
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"{method} {path}: {e.code} {e.read()[:300]!r}")

    def s(self, method, path, body=None, timeout=90):
        return self.call(method, f"/session/{self.session}{path}", body, timeout)

    def js(self, script, *args, timeout=60):
        return self.s("POST", "/execute/sync", {"script": script, "args": list(args)}, timeout)

    def ajs(self, script, *args, timeout=60):
        return self.s("POST", "/execute/async", {"script": script, "args": list(args)}, timeout)

    def close_window_and_wait(self, secs=20):
        try: self.s("DELETE", "/window", timeout=10)
        except Exception as e: pass
        try: return self.proc.wait(secs)
        except subprocess.TimeoutExpired: return None

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill(); self.proc.wait(10)
        self.log.close()


def check_page(binary, name, url, out, work):
    r = {"page": name, "url": url, "checks": {}}
    c = r["checks"]
    profile = os.path.join(work, f"profile-{name}"); os.makedirs(profile, exist_ok=True)
    usdir = os.path.join(work, "us"); log = os.path.join(out, f"{name}.log")
    token = f"desk-{int(time.time())}"; w = None
    sh = Shell(binary, profile, usdir, log)
    try:
        r["start_s"] = sh.start_s
        t0 = time.time(); sh.s("POST", "/url", {"url": url}, timeout=120); r["load_ms"] = round((time.time() - t0) * 1000)
        time.sleep(4)
        info = sh.js("return {ready: document.readyState, title: document.title, canvases: [...document.querySelectorAll('canvas')].map(c => c.width + 'x' + c.height), desk: !!window.__desk}")
        r["page_info"] = info
        c["loads"] = {"pass": info["ready"] == "complete" and (len(info["canvases"]) > 0 or name == "probe"), "detail": info}
        # input: WebDriver actions -> the page's capture listeners
        if not info.get("desk"): sh.js(USERSCRIPT)
        before = sh.js("return {k: __desk.keys, p: __desk.pointers}")
        acts = {"actions": [
            {"type": "key", "id": "kb", "actions": [{"type": "keyDown", "value": " "}, {"type": "pause", "duration": 100}, {"type": "keyUp", "value": " "},
                                                       {"type": "keyDown", "value": ""}, {"type": "pause", "duration": 300}, {"type": "keyUp", "value": ""}]},
            {"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": [
                {"type": "pointerMove", "x": 640, "y": 360, "origin": "viewport"}, {"type": "pointerDown", "button": 0},
                {"type": "pause", "duration": 100}, {"type": "pointerUp", "button": 0}]}]}
        try: sh.s("POST", "/actions", acts)
        except Exception as e: c["input"] = {"pass": False, "detail": str(e)[:300]}
        time.sleep(0.5)
        after = sh.js("return {k: __desk.keys, p: __desk.pointers}")
        if "input" not in c:
            c["input"] = {"pass": after["k"] - before["k"] >= 2 and after["p"] - before["p"] >= 1, "detail": {"keys": after["k"] - before["k"], "pointers": after["p"] - before["p"]}}
        c["fps"] = {"value": sh.ajs(FPS_JS, timeout=30)}
        try: a = sh.ajs(AUDIO_JS, timeout=30)
        except Exception as e: a = {"error": str(e)[:300]}
        c["audio"] = {"pass": a.get("offline_peak", 0) > 0.5, "realtime": a.get("realtime_state") == "running" and a.get("realtime_advanced_s", 0) > 0.5, "detail": a}
        if name == "probe":
            try: c["probe"] = {"detail": sh.ajs("window.__probe().then(arguments[arguments.length-1])", timeout=30)}
            except Exception as e: c["probe"] = {"detail": str(e)[:300]}
        w = sh.ajs(SAVE_JS, token, True, timeout=30)
        errors = sh.js("return __desk.errors")
        c["no_errors"] = {"pass": not errors, "detail": errors[:10]}
        try:
            png = sh.s("GET", "/screenshot", timeout=60)
            with open(os.path.join(out, f"{name}.png"), "wb") as f: f.write(base64.b64decode(png))
        except Exception as e: r["screenshot_error"] = str(e)[:200]
        code = sh.close_window_and_wait(20)
        c["clean_exit"] = {"pass": code == 0, "detail": {"exit_code": code, "how": "WebDriver close of the last window"}}
    except Exception as e:
        r["error"] = str(e)[:500]
        c.setdefault("loads", {"pass": False, "detail": str(e)[:300]})
    finally:
        sh.kill()
    # saves: a fresh process on the same profile reads what the first one wrote
    if c.get("loads", {}).get("pass"):
        sh2 = None
        try:
            sh2 = Shell(binary, profile, usdir, log); sh2.s("POST", "/url", {"url": url}, timeout=120); time.sleep(2)
            rd = sh2.ajs(SAVE_JS, token, False, timeout=30)
            c["saves"] = {"pass": rd.get("local") == token and rd.get("idb") == token, "detail": {"wrote": w, "read": rd}}
        except Exception as e:
            c["saves"] = {"pass": False, "detail": str(e)[:300]}
        finally:
            if sh2: sh2.kill()
    return r


def exit_run(binary, url, out, work):
    """servoshell's own --exit path: load, wait for a stable image, write it, exit 0."""
    t0 = time.time()
    prof = os.path.join(work, "profile-exit"); os.makedirs(prof, exist_ok=True)
    p = subprocess.run([binary, "--headless", "--exit", "-o", os.path.join(out, "exit-run.png"), url], capture_output=True,
                       timeout=180, env=profile_env(prof))
    return {"exit_code": p.returncode, "seconds": round(time.time() - t0, 1), "png": os.path.exists(os.path.join(out, "exit-run.png")),
            "tail": p.stdout.decode(errors="replace")[-500:] + p.stderr.decode(errors="replace")[-500:]}


def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--out", required=True)
    ap.add_argument("pages", nargs="*"); a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True); work = tempfile.mkdtemp(prefix="desk-")
    os.makedirs(os.path.join(work, "us")); open(os.path.join(work, "us", "desk.js"), "w").write(USERSCRIPT)
    site = os.path.join(work, "site"); os.makedirs(site); open(os.path.join(site, "probe.html"), "w").write(PROBE_HTML)
    hp = free_port()
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", hp), lambda *x: http.server.SimpleHTTPRequestHandler(*x, directory=site))
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    pages = [p.split("=", 1) for p in a.pages] or [["probe", f"http://127.0.0.1:{hp}/probe.html"], ["hexii", "https://hexii.partii.games/"]]
    res = {"schema": "partii.desk-check/1", "os": platform.platform(), "machine": platform.machine(), "bin": a.bin, "pages": []}
    for name, url in pages:
        print(f"== {name} {url}", flush=True)
        r = check_page(a.bin, name, url, a.out, work); res["pages"].append(r)
        print(json.dumps(r, indent=1)[:3000], flush=True)
    try: res["exit_run"] = exit_run(a.bin, f"http://127.0.0.1:{hp}/probe.html", a.out, work)
    except Exception as e: res["exit_run"] = {"error": str(e)}
    json.dump(res, open(os.path.join(a.out, "desk-check.json"), "w"), indent=1)
    cell = lambda ch: "" if not ch else ("PASS" if ch.get("pass") else "FAIL")
    md = [f"# desk check on {res['os']} ({res['machine']})", "", "| page | load ms | loads | input | saves | audio (offline / realtime) | no_errors | clean exit | fps |", "|---|---|---|---|---|---|---|---|---|"]
    for r in res["pages"]:
        c = r["checks"]; au = c.get("audio", {})
        md.append(f"| {r['page']} | {r.get('load_ms', '')} | {cell(c.get('loads'))} | {cell(c.get('input'))} | {cell(c.get('saves'))} | "
                  f"{cell(au)} / {'yes' if au.get('realtime') else 'no'} | {cell(c.get('no_errors'))} | {cell(c.get('clean_exit'))} | {c.get('fps', {}).get('value', '')} |")
    md.append(""); md.append(f"--exit run: {json.dumps({k: v for k, v in res['exit_run'].items() if k != 'tail'})}")
    open(os.path.join(a.out, "desk-check.md"), "w").write("\n".join(md) + "\n"); print("\n".join(md))
    srv.shutdown(); shutil.rmtree(work, ignore_errors=True)
    return 0 if all(r["checks"].get("loads", {}).get("pass") for r in res["pages"]) else 1


if __name__ == "__main__":
    sys.exit(main())
