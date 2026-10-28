import html
import http.server
import os
import sys
import time
import urllib.parse

ROOT = os.path.realpath(sys.argv[1])
PORT = int(sys.argv[2])
RELOAD_PATH = "/.zed-live-preview/reload"
EVENTS_PATH = "/.zed-live-preview/events"
# Keep this script byte-identical to RELOAD_SCRIPT in
# crates/web_preview/src/web_preview.rs.
RELOAD_SCRIPT = "<script>(()=>{const base='/.zed-live-preview';const declare=()=>{const q=new URLSearchParams();q.append('p',location.pathname);performance.getEntriesByType('resource').slice(0,63).forEach(e=>{const u=new URL(e.name,location.href);if(u.origin===location.origin)q.append('p',u.pathname)});return q.toString()};const reload=()=>location.reload();if(typeof EventSource!=='function'){let seen='';const poll=()=>{fetch(base+'/reload?'+declare(),{cache:'no-store'}).then(r=>r.text()).then(next=>{if(seen&&seen!==next)reload();seen=next}).catch(()=>{}).finally(()=>setTimeout(poll,1000))};poll();return}let stream=null;let declared='';const connect=()=>{declared=declare();const next=new EventSource(base+'/events?'+declared);next.onmessage=e=>{if(e.data==='reload')reload()};next.onerror=()=>{if(next.readyState===EventSource.CLOSED){next.close();setTimeout(connect,1000)}};if(stream)stream.close();stream=next};const refresh=setInterval(()=>{if(stream&&stream.readyState===EventSource.OPEN&&declare()!==declared)connect()},2000);addEventListener('pagehide',()=>{clearInterval(refresh);if(stream)stream.close()});connect()})()</script>"


def declared_paths(query):
    paths = []
    for requested in urllib.parse.parse_qs(query).get("p", [])[:64]:
        path = os.path.realpath(os.path.join(ROOT, requested.lstrip("/")))
        try:
            if os.path.commonpath((ROOT, path)) == ROOT:
                paths.append(path)
        except OSError:
            pass
    return paths


def revision(paths):
    latest = 0
    for path in paths:
        try:
            latest = max(latest, os.stat(path).st_mtime_ns)
        except OSError:
            pass
    return str(latest)


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def log_message(self, format, *args):
        pass

    def list_directory(self, path):
        self.send_error(403, "Directory listing is disabled")
        return None

    def translate_path(self, path):
        translated = os.path.realpath(super().translate_path(path))
        if os.path.commonpath((ROOT, translated)) != ROOT:
            return os.path.join(ROOT, ".zed-preview-forbidden")
        return translated

    def do_GET(self):
        parsed = urllib.parse.urlsplit(self.path)
        if parsed.path == RELOAD_PATH:
            body = revision(declared_paths(parsed.query)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
            return
        if parsed.path == EVENTS_PATH:
            self.serve_events(declared_paths(parsed.query))
            return
        path = self.translate_path(self.path)
        if os.path.isfile(path) and os.path.splitext(path)[1].lower() in {".html", ".htm"}:
            try:
                with open(path, "rb") as source:
                    body = source.read().decode("utf-8")
                index = body.lower().rfind("</body>")
                if index < 0:
                    index = len(body)
                body = (body[:index] + RELOAD_SCRIPT + body[index:]).encode()
            except (OSError, UnicodeError):
                return super().do_GET()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()

    def serve_events(self, paths):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Accel-Buffering", "no")
        self.end_headers()
        self.wfile.write(b"retry: 1000\n\n")
        last = revision(paths)
        try:
            while True:
                time.sleep(0.25)
                current = revision(paths)
                if current != last:
                    last = current
                    self.wfile.write(b"data: reload\n\n")
                    self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
