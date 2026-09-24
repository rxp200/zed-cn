import html
import http.server
import os
import sys
import urllib.parse

ROOT = os.path.realpath(sys.argv[1])
PORT = int(sys.argv[2])
RELOAD_PATH = "/.zed-live-preview/reload"
RELOAD_SCRIPT = "<script>(()=>{let v=\"\";const poll=()=>{const q=new URLSearchParams();q.append('p',location.pathname);performance.getEntriesByType('resource').slice(0,63).forEach(e=>{const u=new URL(e.name,location.href);if(u.origin===location.origin)q.append('p',u.pathname)});fetch('/.zed-live-preview/reload?'+q,{cache:'no-store'}).then(r=>r.text()).then(n=>{if(v&&v!==n)location.reload();v=n}).catch(()=>{}).finally(()=>setTimeout(poll,700))};poll()})()</script>"


def revision(query):
    latest = 0
    for requested in urllib.parse.parse_qs(query).get("p", [])[:64]:
        relative = requested.lstrip("/")
        path = os.path.realpath(os.path.join(ROOT, relative))
        try:
            if os.path.commonpath((ROOT, path)) == ROOT:
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
            body = revision(parsed.query).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
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


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
