#!/usr/bin/env python3
"""End-to-end check of the real MCP server against emulated providers.

Starts a local HTTP server that answers like every provider spec says its API
answers, points a scratch home at it through spec overrides (https://host ->
http://127.0.0.1:port/h/<key>/host), launches `ai-media-router mcp` over stdio
and drives it the way an agent would: every tool, and one generation through
every route of every provider, plus a replay of each recorded fixture over HTTP.

    python3 scripts/emulate.py [--bin target/release/ai-media-router] [--only kie,fal]

Exit code 1 when anything failed. Standard library only (Python 3.11+).
"""

import argparse
import base64
import fnmatch
import glob
import hashlib
import itertools
import json
import os
import queue
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import traceback
import urllib.error
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, quote, urlsplit

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
)
MP4 = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom" + b"\x00" * 32
GENERATION_STEPS = ("prepare", "submit", "poll", "result")
FFMPEG = os.environ.get("FFMPEG") or shutil.which("ffmpeg")
SEQ = itertools.count()


# ---------------------------------------------------------------- pointers

def split_pointer(pointer):
    return [s.replace("~1", "/").replace("~0", "~") for s in pointer.split("/")[1:]]


def put(node, segments, value):
    """Returns `node` with `value` written at the pointer `segments` (creating containers)."""
    if not segments:
        return value
    seg, rest = segments[0], segments[1:]
    if seg.startswith("*"):
        m = re.fullmatch(r"\*\[([^=\]]+)=([^\]]*)\]", seg)
        items = node if isinstance(node, list) else []
        base = {m.group(1): m.group(2)} if m else {}
        if not items:
            items.append(dict(base) if rest else None)
        first = items[0]
        if rest and isinstance(first, dict):
            first.update(base)
        items[0] = put(first if first is not None else dict(base), rest, value) if rest else value
        return items
    if seg.isdigit():
        items = node if isinstance(node, list) else []
        while len(items) <= int(seg):
            items.append({} if rest else None)
        items[int(seg)] = put(items[int(seg)], rest, value)
        return items
    obj = node if isinstance(node, dict) else {}
    obj[seg] = put(obj.get(seg), rest, value)
    return obj


class Doc:
    """A response being synthesized: JSON body plus headers, starting as the spec's success envelope."""

    def __init__(self, spec):
        self.body = None
        self.headers = {}
        errors = spec.get("errors")
        if errors and "code" in errors:
            self.set(errors["code"], errors["ok"][0])

    def set(self, pointer, value):
        if pointer.startswith("header:"):
            self.headers[pointer[7:]] = str(value)
            return
        outer, hash_, inner = pointer.partition("#")
        if hash_:
            current = self.get(outer)
            doc = json.loads(current) if isinstance(current, str) and current else None
            value = json.dumps(put(doc, split_pointer(inner), value))
        self.body = put(self.body, split_pointer(outer), value) if outer else value

    def get(self, pointer):
        node = self.body
        for seg in split_pointer(pointer):
            if isinstance(node, dict):
                node = node.get(seg)
            elif isinstance(node, list) and seg.isdigit() and int(seg) < len(node):
                node = node[int(seg)]
            else:
                return None
        return node

    def reply(self):
        return Reply(body=self.body if self.body is not None else {}, headers=self.headers)


# ---------------------------------------------------------------- emulator

class Reply:
    def __init__(self, status=200, body=None, headers=None, raw=None, content_type="application/json"):
        self.status, self.body, self.headers, self.raw, self.content_type = status, body, headers or {}, raw, content_type


class Emulator(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server_address[1]}"
        self.lock = threading.Lock()
        self.queues = {}  # provider key -> list of (step, method, Reply)
        self.seen = {}  # provider key -> list of (step, method, path, expected method)
        self.auth_checks = {}  # request path -> (spec [auth] table, env values)
        self.unexpected = []

    def expect(self, key, script):
        with self.lock:
            self.queues[key] = list(script)
            self.seen[key] = []

    def answer_auth_check(self, method, path, headers):
        """A spec's [auth_check]: 200 {} with the spec's credentials, 401 without them, else None."""
        if method != "GET":
            return None
        name = path.split("?")[0]
        with self.lock:
            found = next((a for p, a in self.auth_checks.items() if p == name), None)
        if found is None:
            return None
        auth, env = found
        return Reply(body={}) if authenticated(path, headers, auth, env) else Reply(401, {"error": "invalid api key"})

    def take(self, key, method, path):
        with self.lock:
            q = self.queues.get(key) or []
            if not q and method == "GET":
                # result files behind relative or extension-less URLs (downloaded with credentials)
                self.seen.setdefault(key, []).append(("download", method, path, method))
                return Reply(raw=PNG, content_type="image/png")
            if not q:
                self.unexpected.append(f"{key}: unexpected {method} {path}")
                return Reply(500, {"error": "emulator: no request expected"})
            step, want, reply = q.pop(0)
            self.seen.setdefault(key, []).append((step, method, path, want))
            return reply

    def problems(self, key):
        """Requests sent with a method other than the spec's, and scripted requests never made."""
        with self.lock:
            bad = [f"{step}: sent {got}, spec says {want}" for step, got, _, want in self.seen.get(key, []) if got != want]
            left = [s for s, _, _ in self.queues.get(key) or []]
        if left:
            bad.append(f"never requested: {', '.join(left)}")
        return bad


def authenticated(path, headers, auth, env):
    """Whether a request carries the credentials the spec's `[auth]` table asks for."""
    kind = auth.get("type", "none")
    values = [env.get(name, "") for name in auth.get("env", [])]
    if kind == "none":
        return True
    if not values or not all(values):
        return False
    header = lambda name: headers.get(name, "")
    prefix = auth.get("prefix") or ""
    if kind == "bearer":
        return header("authorization") == f"Bearer {values[0]}"
    if kind == "header":
        return header((auth.get("name") or "Authorization").lower()) == f"{prefix}{values[0]}"
    if kind == "headers":
        names, prefixes = auth.get("names", []), auth.get("prefixes", [])
        return len(names) == len(values) and all(
            header(name.lower()) == f"{prefixes[i] if i < len(prefixes) else ''}{value}"
            for i, (name, value) in enumerate(zip(names, values)))
    if kind == "query":
        query = dict(parse_qsl(urlsplit(path).query))
        return query.get(auth.get("param") or "key") == f"{prefix}{values[0]}"
    if kind == "basic":
        pair = base64.b64encode(f"{values[0]}:{values[1]}".encode()).decode() if len(values) > 1 else ""
        return bool(pair) and header("authorization") == f"Basic {pair}"
    return bool(header("authorization"))  # jwt_hs256 / command: a token we cannot recompute here


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def handle_any(self):
        length = int(self.headers.get("content-length") or 0)
        data = self.rfile.read(length) if length else b""
        path = self.path
        name = path.split("?")[0]
        media = re.search(r"\.(png|jpe?g|webp|gif|mp4|mov|webm|mp3|wav)$", name)
        if path.startswith("/files/") or (self.command == "GET" and media):
            raw, mime = (MP4, "video/mp4") if name.endswith(".mp4") else (PNG, "image/png")
            return self.send(Reply(raw=raw, content_type=mime))
        if path.startswith("/embeddings"):
            texts = json.loads(data)["input"]
            vocab = ["cat", "dog", "neon", "forest", "fox"]
            out = [{"index": i, "embedding": [t.lower().count(w) + 0.01 for w in vocab]} for i, t in enumerate(texts)]
            return self.send(Reply(body={"data": out}))
        m = re.match(r"/h/([a-z0-9-]+)/", path)
        if not m:
            self.server.unexpected.append(f"unrouted {self.command} {path}")
            return self.send(Reply(404, {"error": "unrouted"}))
        answer = self.server.answer_auth_check(self.command, path, self.headers)
        if answer is not None:
            with self.server.lock:
                self.server.seen.setdefault(m.group(1), []).append(("auth_check", self.command, path, "GET"))
            return self.send(answer)
        self.send(self.server.take(m.group(1), self.command, path))

    def send(self, r):
        payload = r.raw if r.raw is not None else (b"" if r.body is None else json.dumps(r.body).encode())
        self.send_response(r.status)
        self.send_header("content-type", r.content_type)
        self.send_header("content-length", str(len(payload)))
        for k, v in r.headers.items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = handle_any


# ---------------------------------------------------------------- account stub

ACCOUNT_TOOLS = [
    {"name": "generate_video", "inputSchema": {
        "type": "object",
        "properties": {"params": {"type": "object", "properties": {"model": {"type": "string"}},
                                  "required": ["model"], "additionalProperties": {}}},
        "required": ["params"]}},
    {"name": "generate_image", "inputSchema": {
        "type": "object",
        "properties": {"params": {"type": "object", "properties": {"model": {"type": "string"}},
                                  "required": ["model"], "additionalProperties": {}}},
        "required": ["params"]}},
    {"name": "job_status", "inputSchema": {
        "type": "object",
        "properties": {"jobId": {"type": "string"}, "sync": {"type": "boolean"}, "raw_data": {"type": "boolean"}},
        "required": ["jobId"]}},
    {"name": "media_upload", "inputSchema": {
        "type": "object",
        "properties": {"filename": {"type": "string"}, "content_type": {"type": "string"}, "files": {"type": "array"}}}},
    {"name": "media_confirm", "inputSchema": {
        "type": "object",
        "properties": {"media_id": {"type": "string"}, "media_ids": {"type": "array"}, "type": {"type": "string"}},
        "required": ["type"]}},
    {"name": "balance", "inputSchema": {"type": "object", "properties": {}}},
]

ACCOUNT_SCOPES = ["openid", "email", "offline_access"]
CALLBACK_PORTS = (41687, 53419, 60871)


class AccountServer(ThreadingHTTPServer):
    """An OAuth authorization + resource server and an MCP tool server, for oauth specs."""

    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), AccountHandler)
        self.base = f"http://127.0.0.1:{self.server_address[1]}"
        self.lock = threading.Lock()
        self.registrations = 0
        self.authorizations = 0
        self.code_exchanges = 0
        self.refreshes = 0
        self.tool_calls = []      # list of (tool name, arguments)
        self.unexpected = []      # list of strings
        self._client_id = None
        self._codes = {}          # code -> {challenge, client_id, redirect_uri, used}
        self._access = set()      # currently valid access tokens
        self._refresh = None      # current refresh token
        self._secrets = []        # every access + refresh token ever issued
        self._uploads = {}        # media_id -> bytes
        self._jobs = {}           # job_id -> "video" | "image"
        self._status_count = {}   # job_id -> job_status calls made
        self._counter = itertools.count(1)
        self._session = None

    def _new_token(self, kind):
        n = next(self._counter)
        tok = f"emu-{kind}-{n}-{secrets.token_hex(6)}"
        self._secrets.append(tok)
        if kind == "at":
            self._access.add(tok)
        else:
            self._refresh = tok
        return tok

    def revoke_access(self):
        with self.lock:
            self._access.clear()

    def assert_no_secrets(self, text):
        with self.lock:
            issued = list(self._secrets)
        for tok in issued:
            need(tok not in text, "an issued token leaked into output")


class AccountHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _body(self):
        length = int(self.headers.get("content-length") or 0)
        return self.rfile.read(length) if length else b""

    def send(self, status, body=None, headers=None, content_type="application/json", raw=None):
        payload = raw if raw is not None else (b"" if body is None else json.dumps(body).encode())
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        self.route()

    def do_POST(self):
        self.route()

    def do_PUT(self):
        self.route()

    def do_DELETE(self):
        self.route()

    def route(self):
        server = self.server
        name = self.path.split("?")[0]
        if name.startswith("/files/"):
            raw, mime = (MP4, "video/mp4") if name.endswith(".mp4") else (PNG, "image/png")
            return self.send(200, raw=raw, content_type=mime)
        if name.startswith("/.well-known/"):
            return self.well_known(name)
        if name == "/oauth/register":
            return self.register()
        if name == "/oauth/authorize":
            return self.authorize()
        if name == "/oauth/token":
            return self.token()
        if name.startswith("/upload/"):
            return self.upload(name[len("/upload/"):])
        if name == "/mcp":
            return self.mcp()
        with server.lock:
            server.unexpected.append(f"unrouted {self.command} {self.path}")
        return self.send(404, {"error": "unrouted"})

    def well_known(self, name):
        server = self.server
        B = server.base
        if name in ("/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-protected-resource"):
            return self.send(200, {
                "resource": B + "/mcp",
                "authorization_servers": [B],
                "scopes_supported": ACCOUNT_SCOPES,
                "bearer_methods_supported": ["header"],
            })
        if name in ("/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"):
            return self.send(200, {
                "issuer": B,
                "authorization_endpoint": B + "/oauth/authorize",
                "token_endpoint": B + "/oauth/token",
                "registration_endpoint": B + "/oauth/register",
                "code_challenge_methods_supported": ["S256"],
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": ACCOUNT_SCOPES,
                "authorization_response_iss_parameter_supported": True,
            })
        return self.send(404, {"error": "not found"})

    def register(self):
        server = self.server
        try:
            req = json.loads(self._body())
        except Exception:
            req = {}
        want = {f"http://127.0.0.1:{p}/callback" for p in CALLBACK_PORTS}
        ok = (set(req.get("redirect_uris") or []) == want
              and req.get("token_endpoint_auth_method") == "none"
              and "authorization_code" in (req.get("grant_types") or [])
              and "refresh_token" in (req.get("grant_types") or []))
        if not ok:
            with server.lock:
                server.unexpected.append(f"register rejected: {req}")
            return self.send(400, {"error": "invalid_client_metadata"})
        with server.lock:
            server.registrations += 1
            server._client_id = f"emu-client-{server.registrations}"
            client_id = server._client_id
        return self.send(201, {
            "client_id": client_id,
            "client_id_issued_at": int(time.time()),
            "redirect_uris": sorted(want),
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
        })

    def authorize(self):
        server = self.server
        B = server.base
        q = dict(parse_qsl(urlsplit(self.path).query))
        redirect_uri = q.get("redirect_uri", "")
        scope = set((q.get("scope") or "").split())
        with server.lock:
            client_id = server._client_id
        problems = []
        if q.get("response_type") != "code":
            problems.append("response_type")
        if not client_id or q.get("client_id") != client_id:
            problems.append("client_id")
        if redirect_uri not in {f"http://127.0.0.1:{p}/callback" for p in CALLBACK_PORTS}:
            problems.append("redirect_uri")
        if q.get("code_challenge_method") != "S256":
            problems.append("code_challenge_method")
        if not q.get("code_challenge"):
            problems.append("code_challenge")
        if not q.get("state"):
            problems.append("state")
        if q.get("resource") != B + "/mcp":
            problems.append("resource")
        if not ({"openid", "email", "offline_access"} <= scope):
            problems.append("scope")
        if problems:
            with server.lock:
                server.unexpected.append(f"authorize invalid ({', '.join(problems)}): {q}")
            return self.send(400, {"error": "invalid_request", "detail": problems})
        code = secrets.token_urlsafe(24)
        with server.lock:
            server.authorizations += 1
            server._codes[code] = {"challenge": q["code_challenge"], "client_id": q["client_id"],
                                   "redirect_uri": redirect_uri, "used": False}
        location = f"{redirect_uri}?code={code}&state={q['state']}&iss={quote(B, safe='')}"
        return self.send(302, headers={"Location": location}, content_type="text/plain", raw=b"")

    def token(self):
        server = self.server
        B = server.base
        params = dict(parse_qsl(self._body().decode()))
        grant = params.get("grant_type")
        if grant == "authorization_code":
            code = params.get("code")
            with server.lock:
                rec = server._codes.get(code)
                current_client = server._client_id
            if (rec is None or rec["used"] or params.get("client_id") != rec["client_id"]
                    or params.get("redirect_uri") != rec["redirect_uri"]
                    or params.get("resource") != B + "/mcp"):
                return self.send(400, {"error": "invalid_grant"})
            verifier = params.get("code_verifier", "")
            challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
            if challenge != rec["challenge"]:
                with server.lock:
                    server.unexpected.append("token: code_verifier does not match the challenge")
                return self.send(400, {"error": "invalid_grant"})
            with server.lock:
                rec["used"] = True
                server.code_exchanges += 1
                access = server._new_token("at")
                refresh = server._new_token("rt")
                aud = rec["client_id"]
            header = base64.urlsafe_b64encode(json.dumps({"alg": "none", "typ": "JWT"}).encode()).rstrip(b"=").decode()
            payload = base64.urlsafe_b64encode(json.dumps({"iss": B, "aud": aud, "email": "emu@example.com"}).encode()).rstrip(b"=").decode()
            return self.send(200, {
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": refresh,
                "scope": "openid email offline_access",
                "id_token": f"{header}.{payload}.",
            })
        if grant == "refresh_token":
            with server.lock:
                current = server._refresh
            if not params.get("refresh_token") or params["refresh_token"] != current or "resource" not in params:
                return self.send(400, {"error": "invalid_grant"})
            with server.lock:
                server.refreshes += 1
                access = server._new_token("at")
                refresh = server._new_token("rt")
            return self.send(200, {
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": refresh,
                "scope": "openid email offline_access",
            })
        return self.send(400, {"error": "unsupported_grant_type"})

    def upload(self, media_id):
        with self.server.lock:
            self.server._uploads[media_id] = self._body()
        return self.send(200, body={})

    def mcp(self):
        server = self.server
        B = server.base
        if self.command == "GET":
            return self.send(405, {"error": "method not allowed"})
        if self.command == "DELETE":
            return self.send(200, body={})
        raw = self._body()  # read before a 401 too: a keep-alive connection must not keep the unread body
        auth = self.headers.get("authorization") or ""
        token = auth[7:] if auth[:7].lower() == "bearer " else ""
        with server.lock:
            ok = token in server._access
        if not ok:
            return self.send(401, {"error": "invalid_token"}, headers={
                "WWW-Authenticate": f'Bearer resource_metadata="{B}/.well-known/oauth-protected-resource/mcp", '
                                    f'scope="openid email offline_access"'})
        try:
            msg = json.loads(raw)
        except Exception:
            with server.lock:
                server.unexpected.append("mcp: body is not JSON")
            return self.send(400, {"error": "parse error"})
        if isinstance(msg, list):
            out, extra = [], {}
            for m in msg:
                if not isinstance(m, dict) or m.get("id") is None:
                    continue
                body, hdrs = self.rpc(m)
                out.append(body)
                extra.update(hdrs)
            if not out:
                return self.send(202, raw=b"")
            return self.send(200, out, headers=extra)
        if not isinstance(msg, dict) or msg.get("id") is None:
            return self.send(202, raw=b"")
        body, hdrs = self.rpc(msg)
        return self.send(200, body, headers=hdrs)

    def rpc(self, msg):
        server = self.server
        mid = msg.get("id")
        method = msg.get("method")
        params = msg.get("params") or {}
        if method == "initialize":
            server._session = secrets.token_hex(8)
            result = {
                "protocolVersion": params.get("protocolVersion") or "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "emu-higgsfield", "version": "1"},
            }
            return {"jsonrpc": "2.0", "id": mid, "result": result}, {"Mcp-Session-Id": server._session}
        if method == "ping":
            return {"jsonrpc": "2.0", "id": mid, "result": {}}, {}
        if method == "tools/list":
            return {"jsonrpc": "2.0", "id": mid, "result": {"tools": ACCOUNT_TOOLS}}, {}
        if method == "tools/call":
            return {"jsonrpc": "2.0", "id": mid, "result": self.call_tool(params)}, {}
        return {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": f"method not found: {method}"}}, {}

    def call_tool(self, params):
        server = self.server
        name = params.get("name")
        args = params.get("arguments") or {}
        with server.lock:
            server.tool_calls.append((name, args))
        if name == "balance":
            return {"content": [{"type": "text", "text": json.dumps({"credits": 100})}], "isError": False}
        structured = None
        problem = None
        if name == "generate_video":
            structured, problem = self.generate(args, "video")
        elif name == "generate_image":
            structured, problem = self.generate(args, "image")
        elif name == "job_status":
            structured = self.job_status(args)
        elif name == "media_upload":
            structured = self.media_upload(args)
        elif name == "media_confirm":
            structured = self.media_confirm(args)
        else:
            return {"content": [{"type": "text", "text": "unknown tool"}], "isError": True}
        if problem:
            return {"content": [{"type": "text", "text": problem}], "isError": True}
        if structured is None:
            return {"content": [{"type": "text", "text": "tool failed"}], "isError": True}
        return {"content": [{"type": "text", "text": json.dumps(structured)}], "structuredContent": structured, "isError": False}

    def generate(self, args, media_type):
        server = self.server
        params = args.get("params") or {}
        model = params.get("model")
        if not model:
            return None, "missing params.model"
        if params.get("use_unlim") is not False:
            with server.lock:
                server.unexpected.append(f"{media_type} use_unlim is not false: {params!r}")
            return None, "use_unlim must be false"
        job_id = str(uuid.uuid4())
        with server.lock:
            server._jobs[job_id] = media_type
        return {"results": [{"id": job_id, "type": media_type, "status": "pending", "model": model}], "cost": 12}, None

    def job_status(self, args):
        server = self.server
        job_id = args.get("jobId")
        if not job_id:
            return None
        with server.lock:
            n = server._status_count.get(job_id, 0) + 1
            server._status_count[job_id] = n
            media_type = server._jobs.get(job_id, "video")
        if n == 1:
            return {"generation": {"id": job_id, "status": "in_progress"}}
        ext = "png" if media_type == "image" else "mp4"
        return {"generation": {"id": job_id, "status": "completed",
                               "results": {"rawUrl": f"{server.base}/files/{job_id}.{ext}"}}}

    def media_upload(self, args):
        server = self.server
        media_id = str(uuid.uuid4())
        return {"uploads": [{"upload_url": f"{server.base}/upload/{media_id}", "media_id": media_id, "method": "PUT",
                             "content_type": args.get("content_type", ""), "instructions": "PUT the bytes"}]}

    def media_confirm(self, args):
        server = self.server
        media_id = args.get("media_id")
        mtype = args.get("type")
        with server.lock:
            uploaded = media_id in server._uploads
        if not uploaded or mtype != "image":
            with server.lock:
                server.unexpected.append(f"media_confirm {media_id!r} type={mtype!r} uploaded={uploaded}")
        return {"results": [{"media_id": media_id, "status": "confirmed", "type": mtype}]}


# ---------------------------------------------------------------- MCP client

class ToolError(Exception):
    pass


class Mcp:
    def __init__(self, cmd, env):
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        self.ids = itertools.count(1)
        self.pending = {}
        self.lock = threading.Lock()
        self.stderr = []
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=lambda: self.stderr.extend(self.proc.stderr), daemon=True).start()
        init = self.request("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "emulate.py", "version": "1"},
        })
        self.server_info = init["serverInfo"]
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _read(self):
        for line in self.proc.stdout:
            msg = json.loads(line)
            if msg.get("id") in self.pending:
                self.pending.pop(msg["id"]).put(msg)

    def _send(self, msg):
        with self.lock:
            self.proc.stdin.write((json.dumps(msg) + "\n").encode())
            self.proc.stdin.flush()

    def request(self, method, params=None, timeout=90):
        rid, box = next(self.ids), queue.Queue()
        self.pending[rid] = box
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        msg = box.get(timeout=timeout)
        if "error" in msg:
            raise RuntimeError(f"{method}: protocol error {msg['error']}")
        return msg["result"]

    def call(self, tool, args=None):
        """Returns the tool's structured result; raises ToolError for tool failures."""
        r = self.request("tools/call", {"name": tool, "arguments": args or {}})
        text = "".join(c.get("text", "") for c in r.get("content", []))
        if r.get("isError"):
            raise ToolError(text)
        return r["structuredContent"] if "structuredContent" in r else json.loads(text)

    def settle(self, tool, args, finished, limit=120):
        """Long-polls `tool` (which blocks up to wait_secs for a change) until `finished(result)`."""
        deadline = time.monotonic() + limit
        while True:
            r = self.call(tool, {**args, "wait_secs": 20})
            if finished(r) or time.monotonic() > deadline:
                return r

    def wait_job(self, job_id):
        return self.settle("get_job", {"job_id": job_id}, lambda j: j["status"] not in ("running", "waiting"))

    def wait_batch(self, batch_id):
        return self.settle("get_batch", {"batch_id": batch_id}, lambda b: not {"running", "waiting"} & b["counts"].keys())

    def run(self, args, tool="generate"):
        return self.wait_job(self.call(tool, args)["id"])

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


# ---------------------------------------------------------------- spec plans

def env_names(text, spec):
    names = set(re.findall(r"\{\{env\.([A-Za-z0-9_]+)\}\}", text))
    for auth in [spec.get("auth", {})] + [r.get("auth") or {} for r in spec.get("route", [])]:
        names.update(auth.get("env", []))
    return names


def rewrite(text, base, key):
    return re.sub(r"https?://", f"{base}/h/{key}/", text)


def oauth_override(text, base):
    """An oauth spec pointed at the account stub's MCP endpoint instead of a rewritten host."""
    url = f"{base}/mcp"
    text = re.sub(r'^([ \t]*base_url[ \t]*=[ \t]*)"[^"]*"', lambda m: f'{m.group(1)}"{url}"', text, count=1, flags=re.M)
    return re.sub(r'^([ \t]*resource[ \t]*=[ \t]*)"[^"]*"', lambda m: f'{m.group(1)}"{url}"', text, count=1, flags=re.M)


def auth_check_path(key, spec):
    """The emulator path a spec's [auth_check] reaches once base_url points at the emulator."""
    url = (spec.get("auth_check") or {}).get("url", "")
    if not url.startswith("/") or "{{" in url:
        return None
    host = re.sub(r"^https?://", "", spec["base_url"]).rstrip("/")
    return f"/h/{key}/{host}{url}"


def model_for(spec, index):
    """A model id that selects route `index` (None = top level), or None if unreachable."""
    routes = spec.get("route", [])
    first_match = lambda m: next((i for i, r in enumerate(routes) if any(fnmatch.fnmatchcase(m, g) for g in r["models"])), None)
    if index is None:
        return "zz-emulated-model" if first_match("zz-emulated-model") is None else None
    return next((m for m in (g.replace("*", "emu") for g in routes[index]["models"]) if first_match(m) == index), None)


def generation_script(spec, emu, key, route=None):
    """Requests the engine should make for one successful generation, in order."""
    steps = {name: step for name in GENERATION_STEPS if (step := (route or {}).get(name) or spec.get(name))}
    templates = json.dumps([s.get("url", "") for s in steps.values()] + [spec.get("base_url", "")])
    n = next(SEQ)
    script = []
    for name, step in steps.items():
        if name == "submit":
            final = bool(step.get("outputs") or step.get("outputs_base64")) or "poll" not in steps
        else:
            final = name == "result" or (name == "poll" and "result" not in steps)
        doc = Doc(spec)
        for var, pointer in step.get("capture", {}).items():
            url_var = re.search(r'"\{\{' + re.escape(var) + r"\}\}", templates)
            doc.set(pointer, f"{emu.base}/h/{key}/captured/{var}" if url_var else f"emu-{var}")
        if name == "poll" and step.get("status"):
            doc.set(step["status"], step["success"][0] if step.get("success") else "done")
        if final:
            for i, pointer in enumerate(step.get("outputs", [])):
                if step.get("output_url"):
                    # an object without a URL field, turned into one by the output_url template
                    fields = {v: f"emu-{v}-{n}-{i}" for v in re.findall(r"\{\{\s*(\w+)\s*\}\}", step["output_url"])}
                    fields.update(re.findall(r"\*\[([^=\]]+)=([^\]]*)\]$", pointer))
                    doc.set(pointer, fields)
                else:
                    doc.set(pointer, f"{emu.base}/files/{key}-{n}-{i}.{'mp4' if i % 2 else 'png'}")
            for pointer in step.get("outputs_base64", []):
                doc.set(pointer, base64.b64encode(PNG).decode())
        if step.get("cost"):
            doc.set(step["cost"], 0.25)
        reply = Reply(raw=MP4, content_type="video/mp4", headers=doc.headers) if step.get("binary") and final else doc.reply()
        method = step.get("method") or ("POST" if name in ("prepare", "submit") else "GET")
        script.append((name, method.upper(), reply))
        if name == "submit" and final:
            break  # synchronous: submit already returned the media, polling is skipped
    return script


def upload_script(spec, emu, key):
    up = spec.get("upload") or {}
    kind = up.get("type")
    if kind in (None, "data_uri"):
        return []
    hosted = f"{emu.base}/files/{key}-uploaded.png"
    doc = Doc(spec)
    if kind in ("multipart", "json"):
        doc.set(up["result"], hosted)
        return [("upload", (up.get("method") or "POST").upper(), doc.reply())]
    for var, pointer in up.get("capture", {}).items():
        doc.set(pointer, {"upload_url": f"{emu.base}/h/{key}/transfer/{var}", "file_url": hosted}.get(var, f"emu-{var}"))
    if up.get("put_headers"):
        doc.set(up["put_headers"], {"x-emulated": "1"})
    if up.get("form_fields"):
        doc.set(up["form_fields"], {"key": "emulated"})
    script = [("upload", (up.get("method") or "POST").upper(), doc.reply()),
              ("upload_transfer", "PUT" if kind == "put" else "POST", Reply(body={}))]
    if up.get("complete"):
        done = Doc(spec)
        if up.get("result"):
            done.set(up["result"], hosted)
        script.append(("upload_complete", (up["complete"].get("method") or "POST").upper(), done.reply()))
    return script


# ---------------------------------------------------------------- checks

class Report:
    def __init__(self):
        self.lock = threading.Lock()
        self.rows = []

    def add(self, area, case, ok, detail=""):
        with self.lock:
            self.rows.append((area, case, ok, detail))
        print(f"{'ok  ' if ok else 'FAIL'} {area:<12} {case:<44} {detail if not ok else ''}"[:300], flush=True)

    def check(self, area, case, fn, problems=lambda: []):
        """Records whether `fn` ran without raising and `problems()` found nothing; returns fn's result."""
        try:
            result = fn()
            bad = problems()
            self.add(area, case, not bad, "; ".join(bad))
            return result
        except Exception as e:  # noqa: BLE001
            self.add(area, case, False, f"{type(e).__name__}: {e}")


def need(cond, msg):
    if not cond:
        raise AssertionError(msg)


def refused(mcp, tool, args, text):
    """Runs a call that must fail with `text` in its message; returns the message."""
    try:
        mcp.call(tool, args)
    except ToolError as e:
        need(text in str(e), str(e)[:300])
        return str(e)
    raise AssertionError(f"{tool} was not refused")


def reimport(mcp, home, asset, job_id, parents):
    """Deletes a generated asset and imports a copy of its file: provenance restores job, prompt and lineage."""
    path = os.path.join(home, "reimport-" + os.path.basename(asset["path"]))
    shutil.copyfile(asset["path"], path)
    mcp.call("delete_asset", {"asset_id": asset["id"]})
    back = mcp.call("import_asset", {"path": path})
    p = back["provenance"]
    need(p and p["job_id"] == job_id and sorted(p["parents"]) == sorted(parents) and back["id"] == asset["id"], back)
    need(back["job_id"] == job_id and back["prompt"] == asset["prompt"] and not back["missing_parents"], back)
    ancestors = {a["asset"]["id"] for a in mcp.call("get_lineage", {"asset_id": back["id"]})["ancestors"]}
    need(set(parents) <= ancestors, ancestors)


def sample(home, name, payload):
    path = os.path.join(home, name)
    with open(path, "wb") as f:
        f.write(PNG + payload.encode())
    return path


def run_generation(mcp, emu, report, spec, key, route_index, upload_asset=None):
    route = spec["route"][route_index] if route_index is not None else None
    model = model_for(spec, route_index)
    label = f"{key} " + (f"route {route_index} ({model})" if route is not None else "top level")
    if model is None:
        report.add("generate", label, True, "unreachable, skipped")
        return
    script = generation_script(spec, emu, key, route)
    inp = {"prompt": "an emulated neon cat"}
    if upload_asset:
        script = upload_script(spec, emu, key) + script
        inp["image_url"] = f"{{{{asset:{upload_asset}}}}}"
        label += " + upload"
    emu.expect(key, script)
    try:
        job = mcp.run({"provider": key, "model": model, "input": inp})
        problems = emu.problems(key)
        if job["status"] != "done":
            problems.insert(0, f"status {job['status']}: {job.get('error') or job.get('progress')}")
        elif not job["assets"]:
            problems.insert(0, "done without assets")
        if problems:
            debug = mcp.call("get_job", {"job_id": job["id"], "debug": True})
            problems.append(f"| trace: {json.dumps(debug.get('trace'))[:400]}")
        report.add("generate", label, not problems, "; ".join(problems))
    except Exception as e:  # noqa: BLE001
        report.add("generate", label, False, f"{type(e).__name__}: {e}")


def run_fixture(mcp, emu, report, key, path):
    with open(path, encoding="utf-8") as f:
        fixture = json.load(f)
    if "upload" in fixture or not fixture.get("model"):
        return
    script = []
    for ex in fixture["exchanges"]:
        resp = ex["response"]
        body = resp.get("body")
        if isinstance(body, (dict, list)):
            body = json.loads(rewrite(json.dumps(body), emu.base, key))
        script.append((ex["step"], ex["request"]["method"].upper(), Reply(resp.get("status", 200), body, resp.get("headers"))))
    emu.expect(key, script)
    expect = fixture.get("expect", {})

    def replay():
        try:
            job = mcp.run({"provider": key, "model": fixture["model"], "input": fixture["input"]})
            outcome, error = job["status"], job.get("error") or ""
        except ToolError as e:
            outcome, error = "failed", str(e)
        if "error" in expect:
            need(outcome == "failed" and expect["error"] in error,
                 f"expected failure containing {expect['error']!r}, got {outcome}: {error[:200]}")
        else:
            need(outcome == "done", f"{outcome}: {error[:300]}")
    report.check("fixture", f"{key}/{os.path.basename(path)[:-5]}", replay, lambda: emu.problems(key))


class Tour:
    """Every non-generation tool, the way an agent chains them, mostly against the kie emulator."""

    def __init__(self, mcp, emu, report, home, kie, plain=None):
        self.mcp, self.emu, self.report, self.home, self.kie, self.plain = mcp, emu, report, home, kie, plain

    def check(self, case, fn, area="tool", methods=False):
        return self.report.check(area, case, fn, (lambda: self.emu.problems("kie")) if methods else (lambda: []))

    def gen(self):
        return generation_script(self.kie, self.emu, "kie")

    def keys(self):
        """list_keys masks every value; test_key spends nothing and sends the spec's [auth_check]."""
        mcp = self.mcp
        keys = mcp.call("list_keys")
        need(keys["config_file"].endswith("config.toml") and keys["hint"], keys)
        by = {k["name"]: k for k in keys["keys"]}
        kie = by.get("KIE_API_KEY") or {}
        need(kie.get("set") and kie.get("source") == "config_file", kie)
        need("kie" in kie.get("providers", []) and "keys set KIE_API_KEY" in kie.get("set_command", ""), kie)
        need(kie.get("masked") == "…-key" and "emu-kie-api-key" not in json.dumps(keys), kie)

        before = len(self.emu.seen.get("kie", []))
        accepted = mcp.call("test_key", {"provider": "kie"})
        need(accepted["result"] == "accepted" and "emu-kie-api-key" not in json.dumps(accepted), accepted)
        sent = self.emu.seen.get("kie", [])[before:]
        need(any(step == "auth_check" for step, *_ in sent), f"test_key sent no [auth_check]: {sent}")
        if self.plain:
            plain = mcp.call("test_key", {"provider": self.plain})
            need(plain["result"] == "not_supported", plain)

    def upload(self):
        return upload_script(self.kie, self.emu, "kie")

    def expect(self, *parts):
        self.emu.expect("kie", [step for part in parts for step in part])

    def expect_pair(self):
        """Two concurrent jobs: both submit before either is polled."""
        a, b = self.gen(), self.gen()
        self.expect(a[:1], b[:1], a[1:], b[1:])

    def library(self, keys):
        mcp, check = self.mcp, self.check
        tools = {t["name"] for t in mcp.request("tools/list")["tools"]}
        check(f"tools/list ({len(tools)} tools)", lambda: need(len(tools) >= 50, sorted(tools)))
        check("serverInfo name", lambda: need(mcp.server_info["name"] == "ai-media-router", mcp.server_info))

        providers = check("list_providers", lambda: mcp.call("list_providers"))
        if providers:
            ours = [p for p in providers["providers"] if p["key"] in keys]
            unconfigured = [p["key"] for p in ours if not p["configured"]]
            check(f"{len(ours)} providers configured", lambda: need(len(ours) == len(keys) and not unconfigured, unconfigured))
            check("all emulated providers use overrides", lambda: need(all(p["source"] == "override" for p in ours), "builtin left"))
            check("no spec_errors", lambda: need(not providers.get("spec_errors"), providers.get("spec_errors")))
            for p in ours:
                check(f"get_provider {p['key']}", lambda k=p["key"]: need(mcp.call("get_provider", {"provider": k})["model_hint"], "no hint"))
        check("list_keys + test_key", self.keys)

        def priced_estimate():
            try:
                offers = mcp.call("search_prices", {"query": "seedance", "configured_only": True, "limit": 3})["offers"]
            except ToolError as e:
                if "download failed" not in str(e):
                    raise
                print("skip tool         search_prices / estimate_cost by offer: catalog unreachable (offline)", flush=True)
                return
            need(offers and offers[0]["offer_id"], "no configured offers")
            o = offers[0]
            est = mcp.call("estimate_cost", {"provider": o["provider_key"], "model": "m", "input": {"duration": 5}, "offer_id": o["offer_id"]})
            need(abs(est["estimate_usd"] - o["usdPerSec"] * 5) < 1e-9 and "input.duration" in est["formula"], est)
        check("search_prices + estimate_cost by offer_id", priced_estimate)
        check("estimate_cost without offer says why", lambda: need(
            mcp.call("estimate_cost", {"provider": "kie", "model": "never-used", "input": {}})["note"], "no note"))

        a = check("import_asset", lambda: mcp.call("import_asset", {"path": sample(self.home, "hero.png", "a"), "tags": "hero", "prompt": "red fox portrait"}))
        b = check("import_asset 2", lambda: mcp.call("import_asset", {"path": sample(self.home, "dog.png", "b"), "prompt": "dog in a forest"}))
        if not (a and b):
            return None
        check("get_asset", lambda: need(mcp.call("get_asset", {"asset_id": a["id"]})["id"] == a["id"], "id"))
        check("update_asset", lambda: need(mcp.call("update_asset", {"asset_id": b["id"], "description": "forest dog"})["description"] == "forest dog", "desc"))
        check("search_assets", lambda: need(mcp.call("search_assets", {"query": "fox"})["assets"][0]["id"] == a["id"], "fox first"))
        check("review_asset approve", lambda: need(mcp.call("review_asset", {"asset_ids": [a["id"]], "status": "approved", "rating": 5})["assets"][0]["status"] == "approved", "status"))
        check("review_asset reject", lambda: need(mcp.call("review_asset", {"asset_ids": [b["id"]], "status": "rejected", "notes": "blurry paws"})["assets"][0]["review_notes"] == "blurry paws", "notes"))
        ids = lambda args: [x["id"] for x in mcp.call("search_assets", args)["assets"]]
        check("search_assets by status", lambda: need(ids({"status": "approved"}) == [a["id"]], "approved only"))
        check("search_assets by min_rating", lambda: need(ids({"min_rating": 5}) == [a["id"]], "rated 5"))
        check("search_assets finds review notes", lambda: need(ids({"query": "blurry"}) == [b["id"]], "notes"))

        def rejected_ref_skipped():
            mcp.call("save_reference", {"name": "gate", "kind": "style", "asset_ids": [b["id"]]})
            refused(mcp, "generate", {"provider": "kie", "model": "m", "input": {"image": "{{ref:gate}}"}}, "no usable assets")
            mcp.call("delete_reference", {"reference": "gate"})
        check("{{ref}} skips rejected assets", rejected_ref_skipped)
        check("similar_assets", lambda: need(mcp.call("similar_assets", {"query": "forest dog", "limit": 1})["assets"][0]["id"] == b["id"], "dog first"))
        check("save_reference", lambda: mcp.call("save_reference", {"name": "hero", "kind": "character", "description": "fox", "asset_ids": [a["id"]]}))
        check("add_to_reference", lambda: need(len(mcp.call("add_to_reference", {"reference": "hero", "asset_ids": [b["id"]], "role": "side"})["assets"]) == 2, "2"))
        check("remove_from_reference", lambda: need(len(mcp.call("remove_from_reference", {"reference": "hero", "asset_ids": [b["id"]]})["assets"]) == 1, "1"))
        check("get_reference", lambda: mcp.call("get_reference", {"reference": "hero"}))
        check("list_references", lambda: need(mcp.call("list_references", {"kind": "character"})["references"], "empty"))
        check("add_to_collection", lambda: mcp.call("add_to_collection", {"collection": "proj", "asset_ids": [a["id"], b["id"]]}))
        check("list_collections", lambda: need(mcp.call("list_collections")["collections"][0]["assets"] == 2, "2 assets"))
        check("remove_from_collection", lambda: mcp.call("remove_from_collection", {"collection": "proj", "asset_ids": [b["id"]]}))
        check("tool error is readable", lambda: refused(mcp, "get_asset", {"asset_id": "nope"}, "no asset"))

        def doctor():
            levels = {c["name"]: c["level"] for c in mcp.call("doctor")["checks"]}
            need(levels.get("providers") == "ok" and levels.get("database") == "ok" and "spec" not in levels, levels)
        check("doctor", doctor)

        def preflight():
            sent = len(self.emu.seen.get("kie", []))
            plan = mcp.call("preflight", {"provider": "kie", "model": "m", "input": {"image": f"{{{{asset:{a['id']}}}}}"}})
            need(plan["ready"] and plan["uploads"] == [a["id"]] and plan["requests"], plan)
            missing = mcp.call("preflight", {"provider": "kie", "model": "m", "input": {"image": "{{asset:nope}}"}})
            need(not missing["ready"] and any(c["name"] == "inputs" and c["level"] == "fail" for c in missing["checks"]), missing)
            need(len(self.emu.seen.get("kie", [])) == sent, "preflight reached the provider")
        check("preflight sends nothing", preflight)

        def resources():
            uris = [r["uri"] for r in mcp.request("resources/list")["resources"]]
            need("ai-media-router://guide" in uris and "ai-media-router://providers/kie" in uris, uris[:5])
            guide = mcp.request("resources/read", {"uri": "ai-media-router://guide"})["contents"][0]["text"]
            need(guide.startswith("---") and "name: ai-media-router" in guide[:40], guide[:80])
        check("resources/list + read", resources, area="protocol")

        check("set_budget", lambda: mcp.call("set_budget", {"name": "session", "limit_usd": 1.0}))
        check("budget refuses overspend", lambda: refused(mcp, "generate", {
            "provider": "kie", "model": "m", "input": {"prompt": "x"}, "budget": "session", "estimated_cost_usd": 5}, "would be exceeded"))
        check("budget refuses when no estimate can be computed", lambda: refused(mcp, "generate", {
            "provider": "kie", "model": "never-used", "input": {"prompt": "x"}, "budget": "session"}, "none could be computed"))
        check("list_budgets", lambda: need(mcp.call("list_budgets")["budgets"][0]["remaining_usd"] == 1.0, "remaining"))

        spec = check("get_spec", lambda: mcp.call("get_spec", {"provider": "kie"}))
        self.error_kinds(spec)
        if spec:
            check("check_spec renders requests", lambda: need(mcp.call("check_spec", {"spec": spec["text"], "model": "m", "input": {"prompt": "x"}})["requests"], "no requests"))
            broken = spec["text"].replace('capture = { id = "/data/taskId" }', 'capture = { id = "data/taskId" }')
            check("check_spec rejects bad pointer", lambda: need(not mcp.call("check_spec", {"spec": broken})["valid"], "accepted"))
            check("save_spec", lambda: need(mcp.call("save_spec", {"spec": spec["text"]})["check"]["valid"], "invalid"))
        return a

    def error_kinds(self, spec):
        """One refused generation per error kind; each must be classified and findable by list_jobs."""
        mcp = self.mcp
        cases = [
            ("moderation (text)", "moderation", 400, {"code": 400, "msg": "Your prompt violates our content policy"}),
            ("auth (401)", "auth", 401, {"code": 401, "msg": "bad key"}),
            ("quota (402)", "quota", 402, {"code": 402, "msg": "pay"}),
            ("quota (insufficient text)", "quota", 200, {"code": 433, "msg": "Insufficient credits"}),
            ("rate_limit (429)", "rate_limit", 429, {"code": 429, "msg": "slow down"}),
            ("invalid_input (422)", "invalid_input", 422, {"code": 422, "msg": "duration must be 5 or 10"}),
            ("not_found (404)", "not_found", 404, {"code": 404, "msg": "no such model"}),
            ("provider_error (500)", "provider_error", 500, {"code": 500, "msg": "internal"}),
            ("spec (pointer mismatch)", "spec", 200, {"code": 200, "data": {"task": "renamed"}}),
        ]
        for label, kind, status, body in cases:
            def classified(kind=kind, status=status, body=body):
                self.expect([("submit", "POST", Reply(status, body))])
                error = refused(mcp, "generate", {"provider": "kie", "model": "m", "input": {"prompt": "x"}}, f"Error kind: {kind} ")
                job_id = re.search(r"Job (\w+) recorded as failed", error).group(1)
                jobs = mcp.call("list_jobs", {"status": "failed", "error_kind": kind, "limit": 50})["jobs"]
                need(job_id in [j["id"] for j in jobs], f"list_jobs {{error_kind: {kind}}} misses {job_id}")
            self.check(label, classified, area="error kind")

        def failed_while_polling():
            self.expect([
                ("submit", "POST", Reply(body={"code": 200, "data": {"taskId": "t-nsfw"}})),
                ("poll", "GET", Reply(body={"code": 200, "data": {"state": "fail", "failMsg": "NSFW content detected"}})),
            ])
            job = mcp.run({"provider": "kie", "model": "m", "input": {"prompt": "x"}})
            need((job["status"], job.get("error_kind"), job.get("retryable")) == ("failed", "moderation", False),
                 f"{job['status']} {job.get('error_kind')} {job.get('error')}")
            need("rewrite the prompt" in job["hint"], job["hint"])
        self.check("moderation (poll failure)", failed_while_polling, area="error kind")

        def unreachable():
            dead = re.sub(r"http://127\.0\.0\.1:\d+", "http://127.0.0.1:9", spec["text"]).replace('key = "kie"', 'key = "kie-dead"')
            mcp.call("save_spec", {"spec": dead})
            try:
                refused(mcp, "generate", {"provider": "kie-dead", "model": "m", "input": {"prompt": "x"}}, "Error kind: network (retryable)")
            finally:
                mcp.call("reset_spec", {"provider": "kie-dead"})
        if spec:
            self.check("network (unreachable)", unreachable, area="error kind")

    def media(self):
        """Media operations on small lavfi-made files."""
        mcp = self.mcp
        check = lambda case, fn: self.check(case, fn, area="media")

        def synth(name, *args):
            path = os.path.join(self.home, name)
            subprocess.run([FFMPEG, "-v", "error", "-y", *args, path], check=True, capture_output=True)
            return mcp.call("import_asset", {"path": path, "prompt": f"media {name}"})

        clip = check("import video", lambda: synth("clip.mp4", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=2",
                                                   "-f", "lavfi", "-i", "sine=duration=2", "-pix_fmt", "yuv420p", "-shortest"))
        silent = check("import silent video", lambda: synth("silent.mp4", "-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=1", "-pix_fmt", "yuv420p"))
        tone = check("import audio", lambda: synth("tone.wav", "-f", "lavfi", "-i", "sine=duration=1"))
        logo = check("import image", lambda: synth("logo.png", "-f", "lavfi", "-i", "color=c=red:size=40x40", "-frames:v", "1"))
        if not (clip and silent and tone and logo):
            return
        check("metadata on import", lambda: need((clip["width"], clip["height"], clip["has_audio"]) == (320, 240, True) and abs(clip["duration_secs"] - 2) < 0.2, clip))

        def op(tool, args, **want):
            r = mcp.call(tool, args)
            need(r["job_id"] and r["asset"]["id"], r)
            for k, v in want.items():
                need(r["asset"][k] == v, f"{k}={r['asset'][k]!r}, want {v!r}")
            return r

        frame = check("extract_frame", lambda: op("extract_frame", {"asset_id": clip["id"], "at": "last", "save_to": {"collection": "cuts"}}, mime="image/png", width=320))
        check("reframe", lambda: op("reframe", {"asset_id": clip["id"], "aspect": "9:16"}, width=134, height=240))
        cut = check("trim", lambda: op("trim", {"asset_id": clip["id"], "start": 0.5, "end": 1.5}, has_audio=True))
        if cut:
            check("video carries provenance", lambda: reimport(mcp, self.home, cut["asset"], cut["job_id"], [clip["id"]]))
        check("concat", lambda: op("concat", {"asset_ids": [clip["id"], silent["id"]]}, width=320, has_audio=True))
        check("overlay", lambda: op("overlay", {"base_asset_id": clip["id"], "overlay_asset_id": logo["id"], "x": 10, "y": 10, "width": 20}, mime="video/mp4"))
        mux = check("mux_audio", lambda: op("mux_audio", {"video_asset_id": silent["id"], "audio_asset_id": tone["id"]}, has_audio=True))
        if frame:
            job = check("local job recorded", lambda: mcp.call("get_job", {"job_id": frame["job_id"]}))
            check("local job fields", lambda: need(job and (job["provider"], job["model"], job["status"]) == ("local", "extract_frame", "done"), job))
            check("save_to on media op", lambda: need(mcp.call("search_assets", {"collection": "cuts"})["assets"][0]["id"] == frame["asset"]["id"], "not filed"))
        if mux:
            ancestors = {a["asset"]["id"] for a in mcp.call("get_lineage", {"asset_id": mux["asset"]["id"]})["ancestors"]}
            check("lineage of media op", lambda: need(ancestors == {silent["id"], tone["id"]}, ancestors))
        check("media error is readable", lambda: refused(mcp, "extract_frame", {"asset_id": logo["id"]}, "needs video input"))

    def generations(self):
        """{{ref}} + save_to, {{job:<id>}} chains, cancel_job, batches, recipes and last_frame on the kie emulator."""
        mcp, check = self.mcp, self.check

        def saved():
            self.expect(self.upload(), self.gen())
            mcp.run({"provider": "kie", "model": "emu", "input": {"image": "{{ref:hero}}"},
                     "save_to": {"collection": "proj", "reference": "hero", "role": "generated"}})
        check("generate with {{ref}} + save_to", saved, methods=True)

        def chain():
            self.expect(self.gen(), self.upload(), self.gen())
            parent = mcp.call("generate", {"provider": "kie", "model": "emu", "input": {"prompt": "still"}})
            child = mcp.call("generate", {"provider": "kie", "model": "emu", "input": {"image": f"{{{{job:{parent['id']}}}}}"}})
            need(child["status"] == "waiting" and child["waiting_on"] == [parent["id"]], child)
            child = mcp.wait_job(child["id"])
            need(child["status"] == "done" and child.get("inputs"), child)
        check("generate {{job}} waits, then submits", chain, methods=True)

        def cancel():
            self.expect(self.gen()[:1])
            job = mcp.call("generate", {"provider": "kie", "model": "emu", "input": {"prompt": "x"}})
            waiting = mcp.call("generate", {"provider": "kie", "model": "emu", "input": {"image": f"{{{{job:{job['id']}}}}}"}})
            need(mcp.call("cancel_job", {"job_id": job["id"]})["error"] == "cancelled", "not cancelled")
            child = mcp.call("get_job", {"job_id": waiting["id"]})
            need(child["status"] == "failed" and "cancelled" in child["error"], child)
        check("cancel_job fails the waiting child", cancel, methods=True)

        def quiet():
            """Nothing reached the kie emulator since the last expect."""
            with self.emu.lock:
                sent = list(self.emu.seen.get("kie", [])) + [u for u in self.emu.unexpected if u.startswith("kie:")]
            need(not sent, f"provider was called: {sent}")

        def reuse():
            args = {"provider": "kie", "model": "emu", "input": {"prompt": "reuse me"}}
            self.expect(self.gen())
            first = mcp.run(args)
            need(first["status"] == "done", first.get("error"))
            self.expect()
            pre = mcp.call("preflight", {**args, "reuse": True})
            need(pre.get("reuse") == {"state": "done", "job_id": first["id"]} and pre["estimate_usd"] == 0, pre)
            again = mcp.call("generate", {**args, "reuse": True})
            need(again["status"] == "done" and again.get("reused_from") == first["id"] and again["cost"] == 0, again)
            need([a["id"] for a in again["assets"]] == [a["id"] for a in first["assets"]], again["assets"])
            quiet()
        check("generate reuse: true returns the earlier outputs, sends nothing", reuse, methods=True)

        def in_flight():
            args = {"provider": "kie", "model": "emu", "input": {"prompt": "slow"}}
            self.expect(self.gen()[:1])
            first = mcp.call("generate", args)
            again = mcp.call("generate", args)
            need(again["id"] == first["id"] and "reuse: false" in again.get("note", ""), again)
            mcp.call("cancel_job", {"job_id": first["id"]})
            with self.emu.lock:
                submits = [s for s in self.emu.seen.get("kie", []) if s[0] == "submit"]
            need(len(submits) == 1, submits)
        check("identical generate while running returns that job", in_flight, methods=True)

        mcp.call("set_budget", {"name": "batch", "limit_usd": 1.0})
        batch = {"defaults": {"provider": "kie", "input": {"prompt": "bench"}, "budget": "batch", "estimated_cost_usd": 0.6,
                              "save_to": {"collection": "bench"}},
                 "items": [{"model": "emu-a"}, {"model": "emu-b"}], "matrix": {"aspect_ratio": ["16:9"]}, "label": "bench"}

        def over_budget():
            self.expect()
            error = refused(mcp, "generate_batch", batch, "would be exceeded")
            need("batch of 2 jobs" in error, error)
        check("generate_batch over budget is refused", over_budget, methods=True)

        def run_batch():
            self.expect_pair()
            batch["defaults"]["estimated_cost_usd"] = 0.3
            started = mcp.call("generate_batch", batch)
            need(len(started["jobs"]) == 2, started)
            done = mcp.wait_batch(started["id"])
            need(done["counts"] == {"done": 2} and len(done["outputs"]) >= 2, done["counts"])
            listed = mcp.call("list_jobs", {"batch": started["id"]})["jobs"]
            need(len(listed) == 2, listed)
        check("generate_batch + get_batch + list_jobs {batch}", run_batch, methods=True)

        self.recipes()
        if FFMPEG:
            check("generate {{job:<id>:last_frame}}", self.last_frame, methods=True)
            check("run_recipe with an edit step", self.edit_chain, methods=True)
        else:
            print("ffmpeg not found: last_frame chain not checked", flush=True)

    def recipes(self):
        """Snippets, text placeholders, recipes and MCP prompts, with real runs through kie."""
        mcp, check = self.mcp, self.check
        builtins = ["product_shoot", "ugc_ad", "character_consistency", "localization_fanout", "fix_provider"]
        names = lambda: [p["name"] for p in mcp.request("prompts/list")["prompts"]]
        prompt = lambda name, args: mcp.request("prompts/get", {"name": name, "arguments": args})["messages"][0]["content"]["text"]
        check("prompts/list built-ins", lambda: need(names()[:5] == builtins, names()))

        def shoot():
            text = prompt("product_shoot", {"product": "hero"})
            need("save_reference" in text and "{{ref:hero}}" in text, text)
        check("prompts/get product_shoot", shoot)
        check("save_snippet", lambda: mcp.call("save_snippet", {"name": "look", "text": "neon rim light"}))
        check("list_snippets", lambda: need(mcp.call("list_snippets")["snippets"][0]["name"] == "look", "missing"))
        check("generate refuses unknown snippet", lambda: refused(mcp, "generate", {"provider": "kie", "model": "emu", "input": {"prompt": "{{snippet:nope}}"}}, "known snippets: look"))
        template = {"prompt": "{{ref:hero.description}} {{var:scene}}, {{snippet:look}}"}
        check("save_recipe", lambda: need("scene" in mcp.call("save_recipe", {"name": "fox scene", "provider": "kie", "model": "emu", "input": template, "save_to": {"collection": "proj"}})["variables"], "no var"))
        check("list_recipes", lambda: need(mcp.call("list_recipes")["recipes"][0]["name"] == "fox scene", "missing"))
        check("get_recipe", lambda: need(mcp.call("get_recipe", {"name": "fox scene"})["input"] == template, "input changed"))
        check("prompts/list has recipe", lambda: need("recipe:fox scene" in names(), names()))
        check("prompts/get recipe", lambda: need("run_recipe" in prompt("recipe:fox scene", {"scene": "in snow"}), "no run_recipe"))
        check("run_recipe refuses missing var", lambda: refused(mcp, "run_recipe", {"name": "fox scene"}, "needs `scene`"))

        def run():
            self.expect(self.gen())
            job = mcp.run({"name": "fox scene", "vars": {"scene": "in snow"}}, tool="run_recipe")
            need(job["status"] == "done", job.get("error"))
            need(job["input"]["prompt"] == "{{ref:hero.description}} in snow, {{snippet:look}}", job["input"])
            trace = json.dumps(mcp.call("get_job", {"job_id": job["id"], "debug": True})["trace"])
            need("fox in snow, neon rim light" in trace, trace[:300])
            return job
        job = check("run_recipe resolves text placeholders", run)
        if job:
            check("save_recipe from_job", lambda: need(mcp.call("save_recipe", {"name": "copy", "from_job": job["id"]})["model"] == "emu", "model"))
            check("delete_recipe", lambda: mcp.call("delete_recipe", {"name": "copy"}))

        def preset():
            mcp.call("save_recipe", {"name": "draft", "provider": "kie", "model": "emu", "input": {"prompt": "x", "aspect_ratio": "16:9"}, "save_to": {"collection": "proj"}})
            self.expect(self.gen())
            job = mcp.run({"recipe": "draft", "input": {"prompt": "a fox"}}, tool="generate")
            need(job["status"] == "done" and job["provider"] == "kie", job.get("error"))
            need(job["input"] == {"prompt": "a fox", "aspect_ratio": "16:9"}, job["input"])
            mcp.call("delete_recipe", {"name": "draft"})
        check("generate {recipe} (a preset)", preset)

        def recipe_batch():
            self.expect_pair()
            started = mcp.call("generate_batch", {"recipe": "fox scene", "matrix": {"scene": ["in snow", "at dusk"]}})
            need(len(started["jobs"]) == 2, started)
            done = mcp.wait_batch(started["id"])
            need(done["counts"] == {"done": 2}, done["counts"])
            prompts = sorted(mcp.call("get_job", {"job_id": j["id"]})["input"]["prompt"] for j in done["jobs"])
            need(prompts == ["{{ref:hero.description}} at dusk, {{snippet:look}}", "{{ref:hero.description}} in snow, {{snippet:look}}"], prompts)
        check("generate_batch {recipe, matrix}", recipe_batch)
        self.chain_recipe(prompt)
        check("delete_snippet", lambda: mcp.call("delete_snippet", {"name": "look"}))

    def chain_recipe(self, prompt):
        """A recipe with two kie steps: checked, priced, run as one batch, and saved again from its lineage."""
        mcp, check = self.mcp, self.check
        steps = [{"name": "still", "provider": "kie", "model": "emu", "input": {"prompt": "{{var:scene}} still"}, "estimated_cost_usd": 0.1},
                 {"name": "clip", "provider": "kie", "model": "emu", "input": {"image": "{{step:still}}"}, "estimated_cost_usd": 0.1}]
        vars = {"scene": "snow"}
        check("save_recipe {steps}", lambda: need(len(mcp.call("save_recipe", {"name": "fox chain", "steps": steps, "save_to": {"collection": "proj"}})["steps"]) == 2, "steps"))
        check("save_recipe refuses a later step", lambda: refused(mcp, "save_recipe", {"name": "bad", "steps": [steps[1]]}, "names no earlier step"))

        def checked():
            sent = len(self.emu.seen.get("kie", []))
            plan = mcp.call("preflight", {"recipe": "fox chain", "vars": vars})
            need(plan["ready"] and abs(plan["estimate_usd"] - 0.2) < 1e-9 and set(plan["input"]) == {"still", "clip"}, plan)
            need(plan["input"]["clip"]["image"] == "<output of step still>", plan["input"])
            est = mcp.call("estimate_cost", {"recipe": "fox chain", "vars": vars})
            need([s["step"] for s in est["steps"]] == ["still", "clip"] and abs(est["estimate_usd"] - 0.2) < 1e-9, est)
            need(len(self.emu.seen.get("kie", [])) == sent, "preflight reached the provider")
        check("preflight + estimate_cost {recipe}", checked)
        check("prompts/get chain recipe", lambda: need("still → clip" in prompt("recipe:fox chain", vars) and "get_batch" in prompt("recipe:fox chain", vars), "no steps"))

        def resource():
            text = mcp.request("resources/read", {"uri": "ai-media-router://recipes/fox%20chain"})["contents"][0]["text"]
            need("{{step:still}}" in text, text[:300])
        check("resource recipes/<chain>", resource)

        def run():
            # the still carries its own job id as provenance, so its bytes are new and clip uploads it
            self.expect(self.gen(), self.upload(), self.gen())
            last = mcp.call("run_recipe", {"name": "fox chain", "vars": vars})
            need(last["step"] == "clip" and last["status"] == "waiting" and last.get("batch_id"), last)
            done = mcp.wait_batch(last["batch_id"])
            need(done["counts"] == {"done": 2} and done["recipe"] == "fox chain", done["counts"])
            need([j["step"] for j in done["jobs"]] == ["still", "clip"], done["jobs"])
            need([a["id"] for a in done["outputs"]] == done["jobs"][1]["outputs"], "outputs are not the final step's")
            return last
        last = check("run_recipe {steps} + get_batch", run, methods=True)
        if last:
            def walked():
                saved = mcp.call("save_recipe", {"name": "walked", "from_job": last["id"], "chain": True})["steps"]
                need(len(saved) == 2 and saved[1]["input"]["image"] == f"{{{{step:{saved[0]['name']}}}}}", saved)
                mcp.call("delete_recipe", {"name": "walked"})
            check("save_recipe {from_job, chain}", walked)

    def edit_chain(self):
        """A recipe whose trim step runs in the worker, then hands its last frame to a kie step."""
        mcp = self.mcp
        path = os.path.join(self.home, "edit-chain.mp4")
        subprocess.run([FFMPEG, "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=2",
                        "-pix_fmt", "yuv420p", path], check=True, capture_output=True)
        clip = mcp.call("import_asset", {"path": path})
        steps = [{"name": "cut", "op": "trim", "input": {"asset_id": clip["id"], "start": "{{var:start}}"}},
                 {"name": "next", "provider": "kie", "model": "emu", "input": {"image": "{{step:cut:last_frame}}"}}]
        mcp.call("save_recipe", {"name": "cut chain", "steps": steps, "variables": {"start": {"default": 0.5}}})
        self.expect(self.upload(), self.gen())
        last = mcp.call("run_recipe", {"name": "cut chain"})
        done = mcp.wait_batch(last["batch_id"])
        need(done["counts"] == {"done": 2}, [j.get("error") for j in done["jobs"]])
        cut = mcp.call("get_job", {"job_id": done["jobs"][0]["id"]})
        need((cut["provider"], cut["model"], cut["step"]) == ("local", "trim", "cut") and cut["assets"], cut)
        need(abs(cut["assets"][0]["duration_secs"] - 1.5) < 0.2, cut["assets"][0])

    def last_frame(self):
        """{{job:<id>:last_frame}}: a local trim job's video hands its last frame to a kie job."""
        mcp = self.mcp
        path = os.path.join(self.home, "chain.mp4")
        subprocess.run([FFMPEG, "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=2",
                        "-pix_fmt", "yuv420p", path], check=True, capture_output=True)
        clip = mcp.call("import_asset", {"path": path})
        cut = mcp.call("trim", {"asset_id": clip["id"], "start": 0.5})
        self.expect(self.upload(), self.gen())
        job = mcp.run({"provider": "kie", "model": "emu", "input": {"image": f"{{{{job:{cut['job_id']}:last_frame}}}}"}})
        need(job["status"] == "done" and job.get("inputs"), job.get("error"))
        ancestors = {a["asset"]["id"] for a in mcp.call("get_lineage", {"asset_id": job["assets"][0]["id"]})["ancestors"]}
        need({job["inputs"][0]["asset_id"], cut["asset"]["id"], clip["id"]} <= ancestors, ancestors)

    def finish(self, a):
        mcp, check = self.mcp, self.check
        jobs = mcp.call("list_jobs", {"limit": 500})["jobs"]
        check(f"list_jobs ({len(jobs)})", lambda: need(jobs, "empty"))
        check("list_jobs by reference", lambda: need(mcp.call("list_jobs", {"reference": "hero"})["jobs"], "none used hero"))
        used = next((j for j in jobs if j.get("inputs") and j.get("save_to") and j["status"] == "done" and j["assets"]), None)
        check("a job with library inputs produced files", lambda: need(used, "none"))
        if used:
            out = used["assets"][0]["id"]
            check("get_lineage", lambda: need(mcp.call("get_lineage", {"asset_id": out})["ancestors"], "no ancestors"))
            check("save_to filed outputs", lambda: need(any(x["id"] == out for x in mcp.call("search_assets", {"collection": "proj", "limit": 500})["assets"]), "not in proj"))

        def export():
            r = mcp.call("export_collection", {"collection": "proj"})
            need(r["assets"] >= 2 and os.path.exists(r["path"]), r)
            return r
        full = check("export_collection", export)

        def export_approved():
            only = mcp.call("export_collection", {"collection": "proj", "only_approved": True})
            need(full and only["assets"] == 1 and only["assets"] < full["assets"], only)
        check("export_collection only_approved", export_approved)

        if used:
            parents = [i["asset_id"] for i in used["inputs"]]
            check("image carries provenance", lambda: reimport(mcp, self.home, used["assets"][0], used["id"], parents))

        def stats():
            models = mcp.call("get_stats", {"provider": "kie"})["models"]
            kinds = {k for m in models for k in m["failure_kinds"]}
            want = {"moderation", "auth", "quota", "rate_limit", "invalid_input", "not_found", "provider_error", "spec"}
            need(models and want <= kinds, kinds)
            need(any(m["done"] for m in models), "no done kie jobs")
            need(mcp.call("get_stats", {"collection": "proj"})["models"], "nothing in proj")
        check("get_stats", stats)
        check("delete_budget", lambda: mcp.call("delete_budget", {"name": "session"}))
        check("delete_reference", lambda: mcp.call("delete_reference", {"reference": "hero"}))
        check("delete_asset", lambda: mcp.call("delete_asset", {"asset_id": a["id"]}))
        check("reset_spec", lambda: need(mcp.call("reset_spec", {"provider": "kie"})["source"] == "builtin", "not builtin"))


# ---------------------------------------------------------------- main

def account_flow(mcp, report, home, bin, stub, key):
    """The OAuth sign-in, MCP-tool generation and spec-tool-check tour for one account provider."""
    def check(case, fn):
        return report.check("account", case, fn)

    env = dict(os.environ, AI_MEDIA_ROUTER_HOME=home)

    def signed_out():
        p = mcp.call("get_provider", {"provider": key})
        need(p.get("signed_in") is False, p)
        need("login higgsfield-account" in (p.get("login_command") or ""), p)
        t = mcp.call("test_key", {"provider": key})
        need(t.get("result") == "not_configured", t)
    check("get_provider signed out", signed_out)

    def refused_signed_out():
        try:
            job = mcp.call("generate", {"provider": key, "model": "seedance_2_5", "input": {"prompt": "x"}})
        except ToolError as e:
            need("login" in str(e), str(e)[:300])
            return
        job = mcp.wait_job(job["id"])
        need(job["status"] == "failed" and "login" in (job.get("error") or ""), job)
    check("generate refused signed out", refused_signed_out)

    def run_login():
        proc = subprocess.Popen([bin, "login", key, "--no-browser", "--store", "file"],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        url = None
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                line = proc.stdout.readline()
                if line == "":
                    break
                if "Open this URL to sign in:" in line:
                    url = line.split("Open this URL to sign in:", 1)[1].strip()
                    break
            need(url, "no sign-in URL printed")
            for _ in range(30):
                try:
                    with urllib.request.urlopen(url, timeout=30) as resp:
                        page = resp.read().decode()
                    break
                except urllib.error.URLError as e:
                    if isinstance(e.reason, ConnectionRefusedError):
                        time.sleep(0.2)
                        continue
                    raise
            else:
                raise AssertionError(f"sign-in URL never answered: {url}")
            need("Signed in" in page, page[:500])
            code = proc.wait(timeout=60)
            need(code == 0, f"login exited {code}: {proc.stderr.read()[:500]}")
        finally:
            if proc.poll() is None:
                proc.kill()

    def first_login():
        run_login()
        need(stub.registrations == 1, f"registrations = {stub.registrations}")
    check("login (browserless)", first_login)

    def signed_in():
        time.sleep(3)  # let the server reload config.toml
        p = mcp.call("get_provider", {"provider": key})
        need(p.get("signed_in") is True, p)
        need(p.get("account") == "emu@example.com", p)
        keys = mcp.call("list_keys")
        row = next((k for k in keys["keys"] if k["name"] == "HIGGSFIELD_ACCOUNT_SESSION"), None) or {}
        need(row.get("signed_in") is True, row)
        need(row.get("masked") is None, row)
        need("login" in (row.get("set_command") or ""), row)
    check("signed in after login", signed_in)

    def test_key_accepted():
        before = len(stub.tool_calls)
        t = mcp.call("test_key", {"provider": key})
        need(t.get("result") == "accepted", t)
        with stub.lock:
            calls = list(stub.tool_calls[before:])
        need(any(name == "balance" for name, _ in calls), calls)
    check("test_key accepted", test_key_accepted)

    def video_generate():
        before = len(stub.tool_calls)
        job = mcp.run({"provider": key, "model": "seedance_2_5",
                       "input": {"prompt": "an emulated paper boat", "duration": 4}})
        need(job["status"] == "done" and len(job["assets"]) == 1, job)
        with stub.lock:
            calls = list(stub.tool_calls[before:])
        gens = [a for n, a in calls if n == "generate_video"]
        need(len(gens) == 1, [n for n, _ in calls])
        params = gens[0].get("params") or {}
        need(params.get("model") == "seedance_2_5" and params.get("use_unlim") is False
             and params.get("prompt") == "an emulated paper boat" and params.get("duration") == 4, params)
        statuses = [a for n, a in calls if n == "job_status"]
        need(len(statuses) >= 2 and all(a.get("jobId") for a in statuses), statuses)
        trace = json.dumps(mcp.call("get_job", {"job_id": job["id"], "debug": True}).get("trace"))
        need("tools/call" in trace, trace[:400])
        stub.assert_no_secrets(trace)
    check("video generate via tools", video_generate)

    def upload_and_image():
        asset = mcp.call("import_asset", {"path": sample(home, "hf-upload.png", "account"), "prompt": "emulated upload"})
        before = len(stub.tool_calls)
        job = mcp.run({"provider": key, "model": "image/nano_banana",
                       "input": {"prompt": "a tiny emulated banana",
                                 "medias": [{"role": "image", "value": f"{{{{asset:{asset['id']}}}}}"}]}})
        need(job["status"] == "done", job)
        with stub.lock:
            calls = list(stub.tool_calls[before:])
        uploads = [a for n, a in calls if n == "media_upload"]
        confirms = [a for n, a in calls if n == "media_confirm"]
        images = [a for n, a in calls if n == "generate_image"]
        need(uploads and confirms and images, [n for n, _ in calls])
        need(confirms[0].get("type") == "image", confirms)
        media_id = confirms[0].get("media_id")
        need(media_id, confirms)
        with stub.lock:
            put = media_id in stub._uploads
        need(put, "media bytes were never PUT")
        params = (images[0].get("params") or {})
        need(params.get("model") == "nano_banana", params)
        medias = params.get("medias") or []
        need(medias and medias[0].get("value") == media_id, medias)
    check("upload + image generate", upload_and_image)

    def refresh():
        stub.revoke_access()
        before = stub.refreshes
        job = mcp.run({"provider": key, "model": "seedance_2_5",
                       "input": {"prompt": "after revoke", "duration": 4}})
        need(job["status"] == "done", job)
        need(stub.refreshes >= before + 1, f"refreshes = {stub.refreshes}")
    check("refresh after revoke", refresh)

    def check_spec_tools():
        spec_text = mcp.call("get_spec", {"provider": key})["text"]
        c = mcp.call("check_spec", {"provider": key})
        need(c.get("valid") is True, c)
        # the fixtures record the real host, not this stub, so cargo test replays them instead
        need(c.get("tool_problems") == [], c)
        candidate = spec_text.replace('jobId = "{{id}}"', 'job_id = "{{id}}"')
        cand = mcp.call("check_spec", {"spec": candidate})
        probs = cand.get("tool_problems") or []
        need(any("job_status" in p and ("jobId" in p or "job_id" in p) for p in probs), probs)
    check("check_spec tool validation", check_spec_tools)

    def logout():
        proc = subprocess.run([bin, "logout", key], env=env, capture_output=True, text=True, timeout=60)
        need(proc.returncode == 0, f"logout exited {proc.returncode}: {proc.stderr[:500]}")
        time.sleep(3)  # let the server reload config.toml
        p = mcp.call("get_provider", {"provider": key})
        need(p.get("signed_in") is False, p)
        try:
            job = mcp.call("generate", {"provider": key, "model": "seedance_2_5", "input": {"prompt": "x"}})
        except ToolError as e:
            need("login" in str(e), str(e)[:300])
        else:
            job = mcp.wait_job(job["id"])
            need(job["status"] == "failed" and "login" in (job.get("error") or ""), job)
        with open(os.path.join(home, "config.toml"), encoding="utf-8") as f:
            stub.assert_no_secrets(f.read())
        stub.assert_no_secrets(b"".join(mcp.stderr).decode(errors="replace"))
    check("logout removes session", logout)

    def second_login():
        run_login()
        need(stub.registrations == 1, f"registrations = {stub.registrations} (client_id should be reused)")
        proc = subprocess.run([bin, "logout", key], env=env, capture_output=True, text=True, timeout=60)
        need(proc.returncode == 0, f"logout exited {proc.returncode}: {proc.stderr[:500]}")
    check("second login reuses client_id", second_login)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    exe = "ai-media-router.exe" if os.name == "nt" else "ai-media-router"
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "release", exe))
    ap.add_argument("--only", help="comma-separated provider keys")
    ap.add_argument("--keep", action="store_true", help="keep the scratch home")
    args = ap.parse_args()
    only = set(args.only.split(",")) if args.only else None

    emu = Emulator()
    threading.Thread(target=emu.serve_forever, daemon=True).start()
    stub = AccountServer()
    threading.Thread(target=stub.serve_forever, daemon=True).start()
    home = tempfile.mkdtemp(prefix="ai-media-router-emulate-")
    os.makedirs(os.path.join(home, "providers"))

    specs = {}
    oauth = []
    env = {"EMBEDDINGS_URL": emu.base, "EMBEDDINGS_MODEL": "emu"}
    for path in sorted(glob.glob(os.path.join(ROOT, "providers", "*.toml"))):
        with open(path, encoding="utf-8") as f:
            text = f.read()
        spec = tomllib.loads(text)
        key = spec["key"]
        if only and key not in only:
            continue
        specs[key] = spec
        if (spec.get("auth") or {}).get("type") == "oauth":
            oauth.append(key)
            with open(os.path.join(home, "providers", f"{key}.toml"), "w", encoding="utf-8", newline="") as f:
                f.write(oauth_override(text, stub.base))
            continue
        for name in re.findall(r"\{\{env\.([A-Za-z0-9_]+)\}\}", spec["base_url"]):
            env[name] = f"{emu.base}/h/{key}/{name.lower()}"  # a self-hosted server's address
        for name in env_names(text, spec):
            env.setdefault(name, "emu-" + name.lower().replace("_", "-"))
        with open(os.path.join(home, "providers", f"{key}.toml"), "w", encoding="utf-8", newline="") as f:
            f.write(rewrite(text, emu.base, key))
        check = auth_check_path(key, spec)
        if check:
            emu.auth_checks[check] = (spec.get("auth", {}), dict(env))
    with open(os.path.join(home, "config.toml"), "w", encoding="utf-8", newline="") as f:
        f.write("[env]\n" + "".join(f"{k} = {json.dumps(v)}\n" for k, v in sorted(env.items())))

    proc_env = {k: v for k, v in os.environ.items() if k not in env}
    proc_env.update({"AI_MEDIA_ROUTER_HOME": home, "RUST_LOG": "warn"})
    report = Report()
    mcp = Mcp([os.path.abspath(args.bin), "mcp"], proc_env)
    print(f"server {mcp.server_info}, emulator {emu.base}, home {home}\n", flush=True)
    try:
        plain = next((k for k, s in sorted(specs.items()) if "auth_check" not in s), None)
        tour = Tour(mcp, emu, report, home, specs.get("kie"), plain)
        hero = tour.library(set(specs) - set(oauth)) if "kie" in specs else None  # account specs start signed out

        def provider(key):
            spec = specs[key]
            for index in [None] + list(range(len(spec.get("route", [])))):
                run_generation(mcp, emu, report, spec, key, index)
            if spec.get("upload") and spec["upload"]["type"] != "data_uri":
                asset = mcp.call("import_asset", {"path": sample(home, f"upload-{key}.png", key), "prompt": f"{key} upload"})
                run_generation(mcp, emu, report, spec, key, None if model_for(spec, None) else 0, asset["id"])
            for path in sorted(glob.glob(os.path.join(ROOT, "providers", "fixtures", key, "*.json"))):
                run_fixture(mcp, emu, report, key, path)
            if hero and key == "kie":
                tour.generations()

        # the media tour and the kie tour (queued behind kie's own routes) overlap the other providers
        tasks = [lambda k=k: provider(k) for k in sorted(specs, key=lambda k: k != "kie") if k not in oauth]
        if hero:
            if FFMPEG:
                tasks.insert(1, tour.media)
            else:
                print("ffmpeg not found: media operations not checked", flush=True)
        with ThreadPoolExecutor(max_workers=12) as pool:
            for f in [pool.submit(t) for t in tasks]:
                f.result()
        if hero:
            tour.finish(hero)
        for key in oauth:
            account_flow(mcp, report, home, os.path.abspath(args.bin), stub, key)
    except Exception:  # noqa: BLE001
        report.add("harness", "crashed", False, traceback.format_exc())
    finally:
        mcp.close()
        emu.shutdown()
        stub.shutdown()

    for line in emu.unexpected:
        report.add("emulator", "unexpected request", False, line)
    for line in stub.unexpected:
        report.add("account", "unexpected request", False, line)
    failed = [r for r in report.rows if not r[2]]
    print(f"\n{len(report.rows) - len(failed)} passed, {len(failed)} failed")
    for area, case, _, detail in failed:
        print(f"  {area} {case}: {detail[:600]}")
    if failed:
        print("\nserver stderr (last lines):\n" + b"".join(mcp.stderr[-20:]).decode(errors="replace"))
    if not args.keep:
        shutil.rmtree(home, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
