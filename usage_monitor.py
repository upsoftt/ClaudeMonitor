#!/usr/bin/env python3
"""Claude Usage Monitor — compact tray overlay with direct API fetch."""

import sys
import os
import json
import hashlib
import subprocess
from http.server import HTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from datetime import datetime
import webbrowser
import urllib.request


import psutil


try:
    from trayconsole_client import TrayConsoleClient
    _trayconsole_available = True
except ImportError:
    _trayconsole_available = False

from PyQt5.QtWidgets import (
    QApplication, QWidget, QLabel, QVBoxLayout, QHBoxLayout,
    QSystemTrayIcon, QMenu, QAction, QPushButton,
)
from PyQt5.QtCore import Qt, QTimer, QThread, pyqtSignal, QPoint, QFileSystemWatcher
from PyQt5.QtGui import QIcon, QPixmap, QPainter, QColor, QFont, QCursor

# When frozen by PyInstaller, resolve paths relative to the EXE location
if getattr(sys, "frozen", False):
    _APP_DIR = Path(sys.executable).parent
else:
    _APP_DIR = Path(__file__).parent

AUTH_FILE         = _APP_DIR / "claude_auth.json"
_STATE_FILE       = _APP_DIR / "window_state.json"
ACCOUNTS_DIR      = _APP_DIR / "accounts"
ACCOUNTS_META     = _APP_DIR / "accounts_meta.json"
COOKIE_BRIDGE_PORT = 19224
CLAUDE_CODE_CREDS = Path.home() / ".claude" / ".credentials.json"

_VENV_PY = _APP_DIR / ".venv" / "Scripts" / "python.exe"
_SUBPROCESS_PY = str(_VENV_PY) if _VENV_PY.exists() else sys.executable


def _find_chrome() -> str | None:
    """Return path to Chrome executable, or None."""
    candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        str(Path(os.environ.get("LOCALAPPDATA", "")) / "Google" / "Chrome" / "Application" / "chrome.exe"),
    ]
    for p in candidates:
        if Path(p).exists():
            return p
    return None


def _get_chrome_profiles() -> list:
    """Return list of Chrome profiles: [{dir, name, email}, ...]."""
    chrome_data = Path(os.environ.get("LOCALAPPDATA", "")) / "Google" / "Chrome" / "User Data"
    if not chrome_data.exists():
        return []
    try:
        state = json.loads((chrome_data / "Local State").read_text(encoding="utf-8"))
        info_cache = state.get("profile", {}).get("info_cache", {})
        profiles = []
        for dir_name, info in info_cache.items():
            profiles.append({
                "dir": dir_name,
                "name": info.get("name", dir_name),
                "email": info.get("user_name", ""),
            })
        # Default first, then alphabetical
        profiles.sort(key=lambda p: (0 if p["dir"] == "Default" else 1, p["dir"]))
        return profiles
    except Exception:
        return []


# ---------------------------------------------------------------------------
# Login script (Playwright — re-login fallback)
# ---------------------------------------------------------------------------

_LOGIN_SCRIPT = (
    'import sys, os\n'
    'from pathlib import Path\n'
    'from playwright.sync_api import sync_playwright\n'
    '\n'
    'auth_file = sys.argv[1]\n'
    '\n'
    '# Windows Chrome user data dir — has saved Google accounts for FedCM\n'
    'CHROME_DATA = Path(os.environ.get("LOCALAPPDATA", "")) / "Google" / "Chrome" / "User Data"\n'
    '\n'
    'def _is_claude_domain(domain):\n'
    '    return "claude" in domain or "anthropic" in domain\n'
    '\n'
    'with sync_playwright() as pw:\n'
    '    ctx = None\n'
    '    used_profile = False\n'
    '\n'
    '    if CHROME_DATA.exists():\n'
    '        try:\n'
    '            ctx = pw.chromium.launch_persistent_context(\n'
    '                user_data_dir=str(CHROME_DATA),\n'
    '                channel="chrome",\n'
    '                headless=False,\n'
    '                args=[\n'
    '                    "--profile-directory=Default",\n'
    '                    "--disable-blink-features=AutomationControlled",\n'
    '                    "--no-first-run",\n'
    '                    "--no-default-browser-check",\n'
    '                    "--disable-features=LockProfileCookieDatabase",\n'
    '                ],\n'
    '                timeout=8000,\n'
    '            )\n'
    '            used_profile = True\n'
    '            print("Chrome profile loaded — Google accounts available", flush=True)\n'
    '        except Exception as e:\n'
    '            print(f"Profile launch failed ({e}), using fresh window", flush=True)\n'
    '            ctx = None\n'
    '\n'
    '    if ctx is None:\n'
    '        # Fallback: fresh Chrome — Google FedCM may still suggest saved accounts\n'
    '        browser = pw.chromium.launch(\n'
    '            channel="chrome",\n'
    '            headless=False,\n'
    '            args=[\n'
    '                "--disable-blink-features=AutomationControlled",\n'
    '                "--no-first-run",\n'
    '                "--no-default-browser-check",\n'
    '            ]\n'
    '        )\n'
    '        ctx = browser.new_context(no_viewport=True)\n'
    '\n'
    '    page = ctx.new_page()\n'
    '    page.add_init_script(\n'
    '        "Object.defineProperty(navigator,\'webdriver\',{get:()=>undefined})"\n'
    '    )\n'
    '\n'
    '    if used_profile:\n'
    '        # Remove claude.ai cookies so we get account picker, not auto-redirect\n'
    '        # (Google cookies stay → FedCM shows saved Google accounts)\n'
    '        all_cookies = ctx.cookies()\n'
    '        non_claude = [c for c in all_cookies if not _is_claude_domain(c.get("domain", ""))]\n'
    '        ctx.clear_cookies()\n'
    '        if non_claude:\n'
    '            ctx.add_cookies(non_claude)\n'
    '\n'
    '    page.goto("https://claude.ai/login", wait_until="domcontentloaded", timeout=15000)\n'
    '    print("Waiting for login...", flush=True)\n'
    '    page.wait_for_url(\n'
    '        lambda u: "claude.ai" in u and "/login" not in u\n'
    '                  and "google.com" not in u and "accounts.google" not in u,\n'
    '        timeout=180000\n'
    '    )\n'
    '    # Wait for sessionKey cookie to appear\n'
    '    page.wait_for_function(\n'
    '        "() => document.cookie.includes(\'sessionKey\')",\n'
    '        timeout=30000\n'
    '    )\n'
    '    ctx.storage_state(path=auth_file)\n'
    '    ctx.close()\n'
    '\n'
    'print("AUTH_SAVED", flush=True)\n'
)


# ---------------------------------------------------------------------------
# Fetch script (curl_cffi — fast, no browser needed)
# ---------------------------------------------------------------------------

_FETCH_SCRIPT = (
    'import json, sys\n'
    'from pathlib import Path\n'
    'from curl_cffi import requests\n'
    '\n'
    'auth_file = sys.argv[1]\n'
    'data = json.loads(Path(auth_file).read_text(encoding="utf-8"))\n'
    '\n'
    'cookies = {}\n'
    'for c in data.get("cookies", []):\n'
    '    if "claude.ai" in c.get("domain", ""):\n'
    '        cookies[c["name"]] = c["value"]\n'
    '\n'
    'if not cookies.get("sessionKey"):\n'
    '    print(json.dumps({"error": "no_session"}))\n'
    '    sys.exit(0)\n'
    '\n'
    'session = requests.Session(impersonate="chrome")\n'
    'for name, value in cookies.items():\n'
    '    session.cookies.set(name, value, domain=".claude.ai")\n'
    '\n'
    'try:\n'
    '    r = session.get("https://claude.ai/api/organizations", timeout=15)\n'
    '    if r.status_code != 200:\n'
    '        print(json.dumps({"error": "session_expired", "status": r.status_code}))\n'
    '        sys.exit(0)\n'
    '    orgs = r.json()\n'
    '    org_uuid = orgs[0]["uuid"] if orgs else None\n'
    '    if not org_uuid:\n'
    '        print(json.dumps({"error": "no_org"}))\n'
    '        sys.exit(0)\n'
    '    r2 = session.get(f"https://claude.ai/api/organizations/{org_uuid}/usage", timeout=15)\n'
    '    if r2.status_code != 200:\n'
    '        print(json.dumps({"error": f"usage_fetch_failed:{r2.status_code}"}))\n'
    '        sys.exit(0)\n'
    '    usage = r2.json()\n'
    '    print(json.dumps({"usage": usage, "org_uuid": org_uuid}))\n'
    'except Exception as e:\n'
    '    print(json.dumps({"error": str(e)}))\n'
)


# ---------------------------------------------------------------------------
# Identity script (get account email/name)
# ---------------------------------------------------------------------------

_IDENTITY_SCRIPT = (
    'import json, sys\n'
    'from pathlib import Path\n'
    'from curl_cffi import requests\n'
    '\n'
    '# capabilities-based plan detection (ordered: most specific first)\n'
    'CAP_PLAN = [\n'
    '    ("claude_max_20", "Max 20"),\n'
    '    ("claude_max_5",  "Max 5"),\n'
    '    ("claude_max",    "Max"),\n'
    '    ("claude_pro",    "Pro"),\n'
    '    ("teams",         "Team"),\n'
    '    ("enterprise",    "Ent"),\n'
    ']\n'
    '\n'
    'auth_file = sys.argv[1]\n'
    'data = json.loads(Path(auth_file).read_text(encoding="utf-8"))\n'
    'cookies = {c["name"]: c["value"] for c in data.get("cookies", [])\n'
    '           if "claude.ai" in c.get("domain", "")}\n'
    'if not cookies.get("sessionKey"):\n'
    '    print(json.dumps({"error": "no_session"}))\n'
    '    sys.exit(0)\n'
    'session = requests.Session(impersonate="chrome")\n'
    'for name, value in cookies.items():\n'
    '    session.cookies.set(name, value, domain=".claude.ai")\n'
    'try:\n'
    '    email, disp_name, plan, uuid = "", "", "", ""\n'
    '    r = session.get("https://claude.ai/api/account", timeout=15)\n'
    '    if r.status_code == 200:\n'
    '        acc = r.json()\n'
    '        email     = acc.get("email_address", "")\n'
    '        disp_name = acc.get("display_name", "") or acc.get("full_name", "")\n'
    '        uuid      = acc.get("uuid", "")\n'
    '    r2 = session.get("https://claude.ai/api/organizations", timeout=15)\n'
    '    if r2.status_code == 200:\n'
    '        orgs = r2.json()\n'
    '        if orgs:\n'
    '            org  = orgs[0]\n'
    '            caps = org.get("capabilities", [])\n'
    '            cap_names = [c if isinstance(c, str) else c.get("name", "") for c in caps]\n'
    '            for cap_key, plan_name in CAP_PLAN:\n'
    '                if cap_key in cap_names:\n'
    '                    plan = plan_name\n'
    '                    break\n'
    '            if not plan and org.get("billing_type") == "stripe_subscription":\n'
    '                plan = "Pro"\n'
    '            if not plan:\n'
    '                plan = "Free"\n'
    '    print(json.dumps({"email": email, "name": disp_name, "plan": plan, "uuid": uuid}))\n'
    'except Exception as e:\n'
    '    print(json.dumps({"error": str(e)}))\n'
)


# ---------------------------------------------------------------------------
# Ping script (sends a tiny message to Claude to start the session timer)
# ---------------------------------------------------------------------------

_PING_SCRIPT = (
    'import json, sys, uuid\n'
    'from pathlib import Path\n'
    'from curl_cffi import requests\n'
    '\n'
    'auth_file = sys.argv[1]\n'
    'data = json.loads(Path(auth_file).read_text(encoding="utf-8"))\n'
    '\n'
    'cookies = {}\n'
    'for c in data.get("cookies", []):\n'
    '    if "claude.ai" in c.get("domain", ""):\n'
    '        cookies[c["name"]] = c["value"]\n'
    '\n'
    'if not cookies.get("sessionKey"):\n'
    '    print(json.dumps({"error": "no_session"}))\n'
    '    sys.exit(0)\n'
    '\n'
    'session = requests.Session(impersonate="chrome")\n'
    'for name, value in cookies.items():\n'
    '    session.cookies.set(name, value, domain=".claude.ai")\n'
    '\n'
    'try:\n'
    '    r = session.get("https://claude.ai/api/organizations", timeout=15)\n'
    '    if r.status_code != 200:\n'
    '        print(json.dumps({"error": "session_expired"}))\n'
    '        sys.exit(0)\n'
    '    org_uuid = r.json()[0]["uuid"]\n'
    '\n'
    '    # Create a temporary conversation\n'
    '    r2 = session.post(\n'
    '        f"https://claude.ai/api/organizations/{org_uuid}/chat_conversations",\n'
    '        json={"name": "", "uuid": str(uuid.uuid4())},\n'
    '        timeout=15,\n'
    '    )\n'
    '    if r2.status_code not in (200, 201):\n'
    '        print(json.dumps({"error": f"create_conv:{r2.status_code}"}))\n'
    '        sys.exit(0)\n'
    '    conv = r2.json()\n'
    '    conv_uuid = conv["uuid"]\n'
    '\n'
    '    # Send a tiny message (this starts the 5h timer)\n'
    '    r3 = session.post(\n'
    '        f"https://claude.ai/api/organizations/{org_uuid}/chat_conversations/{conv_uuid}/completion",\n'
    '        json={\n'
    '            "prompt": "\\n\\nHuman: hi\\n\\nAssistant:",\n'
    '            "model": "claude-sonnet-4-20250514",\n'
    '            "max_tokens_to_sample": 5,\n'
    '        },\n'
    '        timeout=30,\n'
    '    )\n'
    '\n'
    '    # Delete the conversation to clean up\n'
    '    session.delete(\n'
    '        f"https://claude.ai/api/organizations/{org_uuid}/chat_conversations/{conv_uuid}",\n'
    '        timeout=15,\n'
    '    )\n'
    '\n'
    '    print(json.dumps({"ok": True}))\n'
    'except Exception as e:\n'
    '    print(json.dumps({"error": str(e)}))\n'
)


# ---------------------------------------------------------------------------
# Account Manager
# ---------------------------------------------------------------------------

class AccountManager:
    """Manages multiple Claude accounts stored in ACCOUNTS_DIR."""

    def __init__(self):
        ACCOUNTS_DIR.mkdir(exist_ok=True)

    def _load_meta(self) -> dict:
        try:
            return json.loads(ACCOUNTS_META.read_text(encoding="utf-8"))
        except Exception:
            return {"active": None, "accounts": []}

    def _save_meta(self, meta: dict):
        ACCOUNTS_META.write_text(
            json.dumps(meta, indent=2, ensure_ascii=False), encoding="utf-8"
        )

    def get_active_id(self) -> str:
        return self._load_meta().get("active") or ""

    def get_active_file(self) -> Path:
        meta = self._load_meta()
        aid = meta.get("active")
        if aid:
            p = ACCOUNTS_DIR / f"{aid}.json"
            if p.exists():
                return p
        # Legacy fallback
        if AUTH_FILE.exists():
            return AUTH_FILE
        return None

    def get_account_file(self, account_id: str) -> Path:
        return ACCOUNTS_DIR / f"{account_id}.json"

    def get_all(self) -> list:
        """Returns only confirmed (non-pending) accounts."""
        return [a for a in self._load_meta().get("accounts", []) if not a.get("pending")]

    def get_all_including_pending(self) -> list:
        return self._load_meta().get("accounts", [])

    def _find_by_stable_cookies(self, new_cookies: list):
        """Find existing account ID by matching lastActiveOrg (org UUID — unique per account).
        Only matches against confirmed accounts that have an email stored."""
        last_active_org = next(
            (c["value"] for c in new_cookies
             if c.get("name") == "lastActiveOrg" and c.get("value")),
            None,
        )
        if not last_active_org:
            return None

        # Only match confirmed accounts with known email — avoids false positives
        for acc in self.get_all():
            if not acc.get("email"):
                continue
            acc_file = ACCOUNTS_DIR / f"{acc['id']}.json"
            if not acc_file.exists():
                continue
            try:
                data = json.loads(acc_file.read_text(encoding="utf-8"))
                existing_org = next(
                    (c["value"] for c in data.get("cookies", [])
                     if c.get("name") == "lastActiveOrg" and c.get("value")),
                    None,
                )
                if existing_org and existing_org == last_active_org:
                    return acc["id"]
            except Exception:
                continue
        return None

    def confirm_account(self, account_id: str):
        """Mark account as confirmed (no longer pending)."""
        meta = self._load_meta()
        for acc in meta["accounts"]:
            if acc["id"] == account_id:
                acc.pop("pending", None)
                break
        self._save_meta(meta)

    def save_cookies(self, cookies: list) -> tuple:
        """Save cookie list from CookieBridge. Returns (account_id, is_new)."""
        session_key = next(
            (c["value"] for c in cookies if c.get("name") == "sessionKey"), None
        )
        if not session_key:
            return None, False

        storage = {"cookies": cookies, "origins": []}

        # ── Fast local dedup by stable cookies (no network) ──────────
        stable_match = self._find_by_stable_cookies(cookies)
        if stable_match:
            # Known user re-logged in — just refresh their cookies
            acc_file = ACCOUNTS_DIR / f"{stable_match}.json"
            acc_file.write_text(json.dumps(storage, indent=2), encoding="utf-8")
            self.confirm_account(stable_match)   # ensure not pending
            if self._load_meta().get("active") is None:
                self.switch_to(stable_match)
            return stable_match, False

        # ── Check by sessionKey hash (exact same session) ─────────────
        aid = "acc_" + hashlib.md5(session_key.encode()).hexdigest()[:10]
        meta = self._load_meta()
        existing = next((a for a in meta["accounts"] if a["id"] == aid), None)
        is_new = existing is None

        acc_file = ACCOUNTS_DIR / f"{aid}.json"
        acc_file.write_text(json.dumps(storage, indent=2), encoding="utf-8")

        if is_new:
            meta["accounts"].append({
                "id": aid,
                "email": "",
                "name": "",
                "plan": "",
                "pending": True,   # hidden until identity confirmed
                "added_at": datetime.now().isoformat(),
            })

        if meta.get("active") is None:
            meta["active"] = aid

        self._save_meta(meta)
        return aid, is_new

    def switch_to(self, account_id: str):
        meta = self._load_meta()
        meta["active"] = account_id
        self._save_meta(meta)

    def update_info(self, account_id: str, email: str = "", name: str = "",
                    plan: str = "", uuid: str = ""):
        meta = self._load_meta()
        for acc in meta["accounts"]:
            if acc["id"] == account_id:
                if email:
                    acc["email"] = email
                if name:
                    acc["name"] = name
                if plan:
                    acc["plan"] = plan
                if uuid:
                    acc["uuid"] = uuid
                break
        self._save_meta(meta)

    def find_by_uuid(self, uuid: str) -> str:
        """Return account_id that has this Anthropic UUID, or empty string."""
        if not uuid:
            return ""
        for acc in self._load_meta().get("accounts", []):
            if acc.get("uuid") == uuid:
                return acc["id"]
        return ""

    def update_cc_tokens(self, account_id: str, tokens: dict):
        """Store Claude Code OAuth tokens for this account."""
        meta = self._load_meta()
        for acc in meta["accounts"]:
            if acc["id"] == account_id:
                acc["cc_tokens"] = {
                    "accessToken":      tokens.get("accessToken", ""),
                    "refreshToken":     tokens.get("refreshToken", ""),
                    "expiresAt":        tokens.get("expiresAt", 0),
                    "scopes":           tokens.get("scopes", []),
                    "subscriptionType": tokens.get("subscriptionType", ""),
                    "rateLimitTier":    tokens.get("rateLimitTier", ""),
                }
                break
        self._save_meta(meta)

    def get_cc_tokens(self, account_id: str) -> dict:
        """Return stored Claude Code OAuth tokens for this account, or {}."""
        for acc in self._load_meta().get("accounts", []):
            if acc["id"] == account_id:
                return acc.get("cc_tokens", {})
        return {}

    def remove(self, account_id: str):
        meta = self._load_meta()
        meta["accounts"] = [a for a in meta["accounts"] if a["id"] != account_id]
        if meta.get("active") == account_id:
            meta["active"] = meta["accounts"][0]["id"] if meta["accounts"] else None
        self._save_meta(meta)
        f = ACCOUNTS_DIR / f"{account_id}.json"
        f.unlink(missing_ok=True)

    def migrate_legacy(self):
        """Import old claude_auth.json as first account if no accounts exist yet."""
        if AUTH_FILE.exists() and not self.get_all():
            try:
                data = json.loads(AUTH_FILE.read_text(encoding="utf-8"))
                cookies = data.get("cookies", [])
                if cookies:
                    self.save_cookies(cookies)
            except Exception:
                pass


# ---------------------------------------------------------------------------
# CookieBridge server (receives cookies pushed by Chrome extension)
# ---------------------------------------------------------------------------

class CookieBridgeServer(QThread):
    cookies_received = pyqtSignal(list)   # list of cookie dicts

    def __init__(self, parent=None):
        super().__init__(parent)
        self._server = None

    def run(self):
        sig = self.cookies_received

        class _Handler(BaseHTTPRequestHandler):
            def do_OPTIONS(self_h):
                self_h.send_response(200)
                self_h.send_header("Access-Control-Allow-Origin", "*")
                self_h.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                self_h.send_header("Access-Control-Allow-Headers", "Content-Type")
                self_h.end_headers()

            def do_GET(self_h):
                if self_h.path == "/health":
                    body = b'{"ok":true,"app":"ClaudeMonitor"}'
                    self_h.send_response(200)
                    self_h.send_header("Content-Type", "application/json")
                    self_h.send_header("Access-Control-Allow-Origin", "*")
                    self_h.send_header("Content-Length", str(len(body)))
                    self_h.end_headers()
                    self_h.wfile.write(body)
                else:
                    self_h.send_response(404)
                    self_h.end_headers()

            def do_POST(self_h):
                if self_h.path != "/site-cookies/claude":
                    self_h.send_response(404)
                    self_h.end_headers()
                    return
                try:
                    length = int(self_h.headers.get("Content-Length", 0))
                    body = json.loads(self_h.rfile.read(length))
                    self_h.send_response(200)
                    self_h.send_header("Content-Type", "application/json")
                    self_h.send_header("Access-Control-Allow-Origin", "*")
                    self_h.end_headers()
                    self_h.wfile.write(b'{"ok":true}')
                    cookies = body.get("cookies", [])
                    if cookies:
                        sig.emit(cookies)
                except Exception:
                    try:
                        self_h.send_response(400)
                        self_h.end_headers()
                    except Exception:
                        pass

            def log_message(self_h, *args):
                pass  # suppress server logs

        class _Server(HTTPServer):
            allow_reuse_address = True

        try:
            self._server = _Server(("localhost", COOKIE_BRIDGE_PORT), _Handler)
            self._server.serve_forever()
        except OSError:
            pass  # port already in use — extension will still push when server starts

    def stop(self):
        if self._server:
            self._server.shutdown()
            self._server = None


# ---------------------------------------------------------------------------
# Threads
# ---------------------------------------------------------------------------

class LoginThread(QThread):
    """Playwright fallback login."""
    done   = pyqtSignal()
    failed = pyqtSignal(str)

    def __init__(self, account_file: Path, parent=None):
        super().__init__(parent)
        self._account_file = account_file

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _LOGIN_SCRIPT, str(self._account_file)],
                capture_output=True, text=True, timeout=200,
                encoding="utf-8", errors="replace",
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            if proc.returncode != 0:
                self.failed.emit(proc.stderr[-400:] or "failed")
            elif "AUTH_SAVED" in proc.stdout:
                self.done.emit()
            else:
                self.failed.emit("Auth not saved.\n" + proc.stdout[-200:])
        except Exception as e:
            self.failed.emit(str(e))


class FetchThread(QThread):
    result          = pyqtSignal(str, list)   # account_id, models
    error           = pyqtSignal(str, str)    # account_id, error_msg
    session_expired = pyqtSignal(str)         # account_id

    def __init__(self, account_file: Path, account_id: str = "", parent=None):
        super().__init__(parent)
        self._account_file = account_file
        self._account_id = account_id

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _FETCH_SCRIPT, str(self._account_file)],
                capture_output=True, text=True, timeout=30,
                encoding="utf-8", errors="replace",
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            if proc.returncode != 0:
                raise RuntimeError(proc.stderr[-400:] or "subprocess failed")
            if not proc.stdout.strip():
                raise RuntimeError("No output. " + (proc.stderr or "")[-200:])
            raw = json.loads(proc.stdout)

            if "error" in raw:
                err = raw["error"]
                if err in ("no_session", "session_expired"):
                    self.session_expired.emit(self._account_id)
                else:
                    self.error.emit(self._account_id, err)
                return

            self.result.emit(self._account_id, self._parse(raw.get("usage", {})))
        except Exception as exc:
            import traceback
            self.error.emit(self._account_id, f"{exc}\n{traceback.format_exc()[-400:]}")

    # Map API keys → display names
    _CATEGORIES = {
        "five_hour":            ("Сессия",     "session"),
        "seven_day":            ("Все модели", "weekly"),
        "seven_day_opus":       ("Opus",       "opus"),
        "seven_day_sonnet":     ("Sonnet",     "sonnet"),
        "seven_day_cowork":     ("Cowork",     "cowork"),
        "seven_day_oauth_apps": ("OAuth Apps", "oauth"),
    }

    def _parse(self, usage: dict) -> list:
        models = []
        for key, (display_name, _tag) in self._CATEGORIES.items():
            val = usage.get(key)
            if val is None or not isinstance(val, dict):
                continue
            pct = val.get("utilization")
            if pct is None:
                continue
            reset_dt = self._iso(val.get("resets_at", ""))
            models.append({
                "name": display_name,
                "pct": round(float(pct), 1),
                "reset_dt": reset_dt,
            })
        return models

    def _iso(self, s):
        if not s:
            return None
        try:
            dt = datetime.fromisoformat(str(s).replace("Z", "+00:00"))
            if dt.tzinfo is not None:
                dt = dt.astimezone().replace(tzinfo=None)
            return dt
        except (ValueError, AttributeError):
            return None


class IdentityThread(QThread):
    result = pyqtSignal(str, str, str, str, str)   # account_id, email, name, plan, uuid

    def __init__(self, account_file: Path, account_id: str, parent=None):
        super().__init__(parent)
        self._account_file = account_file
        self._account_id = account_id

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _IDENTITY_SCRIPT, str(self._account_file)],
                capture_output=True, text=True, timeout=20,
                encoding="utf-8", errors="replace",
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            if proc.returncode != 0 or not proc.stdout.strip():
                return
            raw = json.loads(proc.stdout)
            if "error" not in raw:
                self.result.emit(
                    self._account_id,
                    raw.get("email", ""),
                    raw.get("name", ""),
                    raw.get("plan", ""),
                    raw.get("uuid", ""),
                )
        except Exception:
            pass


class PingThread(QThread):
    done  = pyqtSignal()
    error = pyqtSignal(str)

    def __init__(self, account_file: Path, parent=None):
        super().__init__(parent)
        self._account_file = account_file

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _PING_SCRIPT, str(self._account_file)],
                capture_output=True, text=True, timeout=45,
                encoding="utf-8", errors="replace",
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            if proc.returncode != 0:
                self.error.emit(proc.stderr[-300:] or "ping failed")
                return
            raw = json.loads(proc.stdout) if proc.stdout.strip() else {}
            if "error" in raw:
                self.error.emit(raw["error"])
            else:
                self.done.emit()
        except Exception as e:
            self.error.emit(str(e))


class IncidentFetchThread(QThread):
    result = pyqtSignal(list)
    error = pyqtSignal(str)

    def run(self):
        try:
            url = "https://status.claude.com/api/v2/incidents/unresolved.json"
            req = urllib.request.Request(url)
            with urllib.request.urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read().decode())
            incidents = []
            for inc in data.get("incidents", []):
                updates = inc.get("incident_updates", [])
                last_body = updates[0].get("body", "") if updates else ""
                incidents.append({
                    "id": inc.get("id", ""),
                    "name": inc.get("name", ""),
                    "status": inc.get("status", ""),
                    "impact": inc.get("impact", "none"),
                    "shortlink": inc.get("shortlink", ""),
                    "last_update_body": last_body,
                })
            self.result.emit(incidents)
        except Exception as e:
            self.error.emit(str(e))


# ---------------------------------------------------------------------------
# Tray icon
# ---------------------------------------------------------------------------

def _make_pct_icon(pct) -> QIcon:
    sz = 64
    px = QPixmap(sz, sz)
    px.fill(Qt.transparent)
    p = QPainter(px)
    p.setRenderHint(QPainter.Antialiasing)
    p.setRenderHint(QPainter.TextAntialiasing)
    p.setBrush(QColor(0, 0, 0))
    p.setPen(Qt.NoPen)
    p.drawRoundedRect(0, 0, sz, sz, 4, 4)
    if pct is None:
        p.setPen(QColor("#4ade80"))
        p.setFont(QFont("Arial", 34, QFont.Bold))
        p.drawText(px.rect(), Qt.AlignCenter, "C")
    else:
        color = QColor("#4ade80") if pct < 70 else QColor("#facc15") if pct < 90 else QColor("#f87171")
        p.setPen(color)
        text = f"{pct:.0f}"
        font_size = 38 if len(text) <= 2 else 28
        p.setFont(QFont("Arial", font_size, QFont.Bold))
        p.drawText(px.rect(), Qt.AlignCenter, text)
    p.end()
    return QIcon(px)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_SS_BTN = (
    "QPushButton{color:#ccc;border:none;font-size:12px;background:transparent;padding:0;}"
    "QPushButton:hover{color:#fff;}"
)
_SS_DIM  = "color:#999;font-size:11px;"
_SS_LOGIN_LBL = "color:#777;font-size:10px;"
_SS_LOGIN_BTN = (
    "QPushButton{background:#ff7a2f;color:#fff;border:none;"
    "border-radius:5px;padding:4px 8px;font-size:10px;}"
    "QPushButton:hover{background:#e06a28;}"
)
_SS_SWITCH_BTN = (
    "QPushButton{color:#60a5fa;border:none;font-size:11px;background:transparent;padding:0;}"
    "QPushButton:hover{color:#93c5fd;}"
)
_SS_ADD_ACC_BTN = (
    "QPushButton{background:#1e3a5f;color:#60a5fa;border:1px solid #2d5a8e;"
    "border-radius:3px;padding:2px 6px;font-size:10px;}"
    "QPushButton:hover{background:#2d5a8e;}"
)


def _fmt_remaining(reset_dt) -> str:
    if not reset_dt:
        return ""
    secs = int((reset_dt - datetime.now()).total_seconds())
    if secs <= 0:
        return "сброс"
    if secs < 86400:
        h = secs // 3600
        m = (secs % 3600) // 60
        return f"{h}ч {m}м" if h > 0 else f"{m}м"
    d = secs // 86400
    h = (secs % 86400) // 3600
    return f"{d}д {h}ч"


def _pct_color(pct: float) -> str:
    if pct < 70:
        return "#4ade80"
    return "#facc15" if pct < 90 else "#f87171"


# ---------------------------------------------------------------------------
# Account row widget
# ---------------------------------------------------------------------------

# Fixed column widths — identical across all row states
_COL_CHK  = 22   # checkbox indicator
_COL_NAME = 80   # login (before @)
_COL_PLAN = 42   # Free / Pro / Max / Max 20
_COL_SESS = 70   # 5h session
_COL_WEEK = 70   # 7d limit

_SS_CHK_ACTIVE = (
    "QLabel{color:#4ade80;font-size:12px;font-weight:bold;"
    "border:1px solid #4ade80;border-radius:2px;"
    "padding:0px 2px;background:rgba(74,222,128,15);}"
)
_SS_CHK_INACTIVE = (
    "QPushButton{color:#555;font-size:11px;border:1px solid #444;"
    "border-radius:2px;background:transparent;padding:0px 2px;}"
    "QPushButton:hover{border-color:#60a5fa;color:#60a5fa;background:rgba(96,165,250,10);}"
)


def _login_display(raw: str) -> str:
    """Strip @domain from email, return just the login part."""
    if not raw:
        return "\u2026"
    return raw.split("@")[0] if "@" in raw else raw


class _AccountRow(QWidget):
    switch_requested = pyqtSignal(str)
    remove_requested = pyqtSignal(str)

    def __init__(self, account_id: str, display: str, is_active: bool,
                 plan: str = "",
                 session_pct=None, session_time=None,
                 weekly_pct=None, weekly_time=None,
                 error: str = None, parent=None):
        super().__init__(parent)
        self.account_id = account_id
        self._is_active = is_active
        self._session_reset_dt = None   # stored for live tick updates
        self._weekly_reset_dt  = None

        layout = QHBoxLayout(self)
        layout.setContentsMargins(2, 2, 2, 2)
        layout.setSpacing(0)

        # ── Checkbox ──────────────────────────────────────
        if is_active:
            chk = QLabel("\u2713")
            chk.setStyleSheet(_SS_CHK_ACTIVE)
            chk.setFixedSize(_COL_CHK, 16)
            chk.setAlignment(Qt.AlignCenter)
        else:
            chk = QPushButton(" ")
            chk.setFixedSize(_COL_CHK, 16)
            chk.setStyleSheet(_SS_CHK_INACTIVE)
            chk.setToolTip("Переключиться на этот аккаунт")
            chk.setCursor(QCursor(Qt.PointingHandCursor))
            chk.clicked.connect(lambda: self.switch_requested.emit(account_id))
        layout.addWidget(chk)
        layout.addSpacing(4)

        # ── Login name ────────────────────────────────────
        login = _login_display(display)
        short = (login[:12] + "\u2026") if len(login) > 12 else login
        lbl_name = QLabel(short)
        lbl_name.setStyleSheet(
            f"color:{'#e8e8e8' if is_active else '#bbb'};font-size:11px;"
            + ("font-weight:600;" if is_active else "")
        )
        lbl_name.setFixedWidth(_COL_NAME)
        layout.addWidget(lbl_name)

        # ── Plan ──────────────────────────────────────────
        plan_colors = {"Pro": "#a78bfa", "Max": "#60a5fa", "Max 20": "#38bdf8",
                       "Free": "#6b7280", "Team": "#f59e0b", "Enterprise": "#f59e0b"}
        pc = plan_colors.get(plan, "#6b7280")
        lbl_plan = QLabel(plan or "")
        lbl_plan.setStyleSheet(f"color:{pc};font-size:10px;")
        lbl_plan.setFixedWidth(_COL_PLAN)
        layout.addWidget(lbl_plan)

        # ── Session & Weekly ──────────────────────────────
        if error:
            err_text = {"exp": "сессия истекла", "timeout": "таймаут", "err": "ошибка"}.get(error, "ошибка")
            err_col  = {"exp": "#f87171", "timeout": "#facc15", "err": "#fb923c"}.get(error, "#fb923c")
            lbl_err = QLabel(err_text)
            lbl_err.setStyleSheet(f"color:{err_col};font-size:10px;font-style:italic;")
            lbl_err.setFixedWidth(_COL_SESS + _COL_WEEK)
            layout.addWidget(lbl_err)
        else:
            if session_pct is not None:
                sc   = _pct_color(session_pct)
                stxt = f"{session_pct:.0f}%" + (f" ({session_time})" if session_time else "")
            else:
                sc, stxt = "#555", "\u2026"
            self._lbl_sess = QLabel(stxt)
            self._lbl_sess.setStyleSheet(f"color:{sc};font-size:11px;")
            self._lbl_sess.setFixedWidth(_COL_SESS)
            self._session_pct = session_pct
            layout.addWidget(self._lbl_sess)

            if weekly_pct is not None:
                wc   = _pct_color(weekly_pct)
                wtxt = f"{weekly_pct:.0f}%" + (f" ({weekly_time})" if weekly_time else "")
            else:
                wc, wtxt = "#555", "\u2026"
            self._lbl_week = QLabel(wtxt)
            self._lbl_week.setStyleSheet(f"color:{wc};font-size:11px;")
            self._lbl_week.setFixedWidth(_COL_WEEK)
            self._weekly_pct = weekly_pct
            layout.addWidget(self._lbl_week)

    def tick_timers(self):
        """Called every second to update countdown labels without a full rebuild."""
        if hasattr(self, "_lbl_sess") and self._session_reset_dt is not None:
            t = _fmt_remaining(self._session_reset_dt)
            pct = self._session_pct
            sc = _pct_color(pct) if pct is not None else "#555"
            stxt = (f"{pct:.0f}%" if pct is not None else "…") + (f" ({t})" if t else "")
            self._lbl_sess.setText(stxt)
            self._lbl_sess.setStyleSheet(f"color:{sc};font-size:11px;")
        if hasattr(self, "_lbl_week") and self._weekly_reset_dt is not None:
            t = _fmt_remaining(self._weekly_reset_dt)
            pct = self._weekly_pct
            wc = _pct_color(pct) if pct is not None else "#555"
            wtxt = (f"{pct:.0f}%" if pct is not None else "…") + (f" ({t})" if t else "")
            self._lbl_week.setText(wtxt)
            self._lbl_week.setStyleSheet(f"color:{wc};font-size:11px;")

    def contextMenuEvent(self, e):
        menu = QMenu(self)
        menu.setStyleSheet(
            "QMenu{background:#1a1a1a;border:1px solid #333;color:#ccc;font-size:11px;}"
            "QMenu::item:selected{background:#333;}"
        )
        if not self._is_active:
            menu.addAction("Переключиться на этот аккаунт",
                           lambda: self.switch_requested.emit(self.account_id))
            menu.addSeparator()
        menu.addAction("Удалить из списка",
                       lambda: self.remove_requested.emit(self.account_id))
        menu.exec_(e.globalPos())


# ---------------------------------------------------------------------------
# Overlay window
# ---------------------------------------------------------------------------

class UsageWindow(QWidget):

    def __init__(self, tray: QSystemTrayIcon, account_manager: AccountManager):
        super().__init__()
        self._tray = tray
        self._account_manager = account_manager
        self._drag = QPoint()
        self._drag_started = False
        self._just_compacted = False
        self._rows: list = []
        self._models: list = []
        self._compact_mode = False
        self._logging_in = False
        self._known_incidents = {}
        self._first_incident_fetch = True
        self._toasts = []
        self._incident_thread = None
        self._accounts_data: dict = {}    # account_id → list[model_dict]
        self._accounts_errors: dict = {}   # account_id → "exp" | "err" | "timeout"
        self._fetching_ids: set = set()    # accounts currently being fetched (no double-fetch)
        self._bg_threads: list = []
        self._identity_threads: list = []
        self._waiting_for_bridge = False
        self._bridge_timeout_t = None
        self._rebuild_debounce_t: QTimer = None  # debounce for _rebuild_accounts
        # Auto-kickstart: send a tiny ping when session resets to 0%
        self._autopin_queue: list = []          # account IDs pending auto-ping
        self._autopin_cooldown: dict = {}       # {account_id: datetime} last ping sent
        self._autopin_last_sent = None          # datetime of last ping sent to any account
        self._autopin_threads: list = []        # keep PingThread refs alive
        self._autopin_scheduled = False         # guard: only one _fire_next_autopin in flight

        self.setWindowFlags(
            Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool
        )
        self.setAttribute(Qt.WA_TranslucentBackground)
        self.setAttribute(Qt.WA_DeleteOnClose, False)

        self._build()
        self._init_timers()

        active_file = self._account_manager.get_active_file()
        if active_file:
            self._fetch()
        else:
            self._show_login()

    # ---- build ---------------------------------------------------

    def _build(self):
        outer = QVBoxLayout(self)
        outer.setContentsMargins(5, 5, 5, 5)

        self._card = _Card(self)
        # Width = CHK+4 + NAME + PLAN + SESS + WEEK + paddings
        self._card.setMinimumWidth(
            _COL_CHK + 4 + _COL_NAME + _COL_PLAN + _COL_SESS + _COL_WEEK + 30
        )
        outer.addWidget(self._card)

        vbox = QVBoxLayout(self._card)
        vbox.setContentsMargins(10, 7, 10, 8)
        vbox.setSpacing(3)

        # ── Full header (click anywhere to compact) ─────
        self._header_w = _ClickableWidget()
        self._header_w.clicked.connect(self._enter_compact)
        self._header_w.setCursor(QCursor(Qt.PointingHandCursor))
        h = QHBoxLayout(self._header_w)
        h.setContentsMargins(0, 0, 0, 0)
        h.setSpacing(4)

        self._status_lbl = QLabel()
        self._status_lbl.setStyleSheet(_SS_DIM)
        h.addWidget(self._status_lbl)
        h.addStretch()

        self._btn_ref = QPushButton("\u27f3")
        self._btn_ref.setFixedSize(16, 16)
        self._btn_ref.setStyleSheet(_SS_BTN)
        self._btn_ref.setToolTip("Обновить")
        self._btn_ref.clicked.connect(self._fetch)
        h.addWidget(self._btn_ref)

        self._btn_min = QPushButton("\u2013")
        self._btn_min.setFixedSize(16, 16)
        self._btn_min.setStyleSheet(_SS_BTN)
        self._btn_min.setToolTip("Свернуть в трей")
        self._btn_min.clicked.connect(self.hide_to_tray)
        h.addWidget(self._btn_min)

        self._btn_close = QPushButton("\u00d7")
        self._btn_close.setFixedSize(16, 16)
        self._btn_close.setStyleSheet(_SS_BTN)
        self._btn_close.setToolTip("Закрыть")
        self._btn_close.clicked.connect(QApplication.instance().quit)
        h.addWidget(self._btn_close)

        vbox.addWidget(self._header_w)

        # ── Compact header (hidden by default) ──────────
        self._compact_w = QWidget()
        c = QHBoxLayout(self._compact_w)
        c.setContentsMargins(0, 0, 0, 0)
        c.setSpacing(6)

        self._compact_lbl = QLabel("\u2026")
        self._compact_lbl.setStyleSheet("color:#e0e0e0;font-size:11px;font-weight:600;")
        c.addWidget(self._compact_lbl)

        self._btn_compact_tray = QPushButton("\u2013")
        self._btn_compact_tray.setFixedSize(16, 16)
        self._btn_compact_tray.setStyleSheet(_SS_BTN)
        self._btn_compact_tray.setToolTip("Свернуть в трей")
        self._btn_compact_tray.clicked.connect(self.hide_to_tray)
        c.addWidget(self._btn_compact_tray)

        self._compact_w.hide()
        vbox.addWidget(self._compact_w)

        # ── Body (model rows for active account) ────────
        self._body_w = QWidget()
        self._body = QVBoxLayout(self._body_w)
        self._body.setContentsMargins(0, 0, 0, 0)
        self._body.setSpacing(2)

        self._placeholder = QLabel("загрузка\u2026")
        self._placeholder.setStyleSheet("color:#aaa;font-size:11px;")
        self._placeholder.setAlignment(Qt.AlignCenter)
        self._body.addWidget(self._placeholder)

        vbox.addWidget(self._body_w)

        # ── Incidents section ────────────────────────────
        self._incidents_w = QWidget()
        self._incidents_layout = QVBoxLayout(self._incidents_w)
        self._incidents_layout.setContentsMargins(0, 4, 0, 0)
        self._incidents_layout.setSpacing(2)
        self._incidents_w.hide()
        vbox.addWidget(self._incidents_w)

        # ── Accounts section ─────────────────────────────
        self._accounts_w = QWidget()
        acc_vbox = QVBoxLayout(self._accounts_w)
        acc_vbox.setContentsMargins(0, 6, 0, 0)
        acc_vbox.setSpacing(2)

        # Section header: "Аккаунты" + "+" button
        acc_hdr = QHBoxLayout()
        acc_hdr.setContentsMargins(0, 0, 0, 0)
        acc_hdr.setSpacing(4)

        sep_top = QLabel()
        sep_top.setFixedHeight(1)
        sep_top.setStyleSheet("background:#2a2a2a;")
        acc_vbox.addWidget(sep_top)

        lbl_acc_hdr = QLabel("Аккаунты")
        lbl_acc_hdr.setStyleSheet("color:#888;font-size:9px;letter-spacing:1px;")
        acc_hdr.addWidget(lbl_acc_hdr)
        acc_hdr.addStretch()

        self._btn_add_acc = QPushButton("+ Добавить")
        self._btn_add_acc.setStyleSheet(_SS_ADD_ACC_BTN)
        self._btn_add_acc.setCursor(QCursor(Qt.PointingHandCursor))
        self._btn_add_acc.setToolTip("Добавить аккаунт через браузер")
        self._btn_add_acc.clicked.connect(self._add_account_playwright)
        acc_hdr.addWidget(self._btn_add_acc)

        acc_vbox.addLayout(acc_hdr)

        # Column headers
        col_hdr = QHBoxLayout()
        col_hdr.setContentsMargins(3, 0, 3, 0)
        col_hdr.setSpacing(4)
        for text, width in [
            ("", _COL_CHK + 4), ("Логин", _COL_NAME), ("Тариф", _COL_PLAN),
            ("5ч сессия", _COL_SESS), ("7д лимит", _COL_WEEK),
        ]:
            lbl = QLabel(text)
            lbl.setStyleSheet("color:#888;font-size:10px;")
            lbl.setFixedWidth(width)
            col_hdr.addWidget(lbl)
        acc_vbox.addLayout(col_hdr)

        # Rows container
        self._acc_rows_w = QWidget()
        self._acc_rows_layout = QVBoxLayout(self._acc_rows_w)
        self._acc_rows_layout.setContentsMargins(0, 0, 0, 0)
        self._acc_rows_layout.setSpacing(1)
        acc_vbox.addWidget(self._acc_rows_w)

        # Bridge status label (hidden normally)
        self._bridge_status_lbl = QLabel()
        self._bridge_status_lbl.setStyleSheet("color:#60a5fa;font-size:10px;")
        self._bridge_status_lbl.setWordWrap(True)
        self._bridge_status_lbl.hide()
        acc_vbox.addWidget(self._bridge_status_lbl)

        vbox.addWidget(self._accounts_w)

        self.adjustSize()

    # ---- timers --------------------------------------------------

    def _init_timers(self):
        self._tick_t = QTimer(self)
        self._tick_t.timeout.connect(self._tick)
        self._tick_t.start(1_000)

        self._refresh_t = QTimer(self)
        self._refresh_t.timeout.connect(self._auto_refresh)
        self._refresh_t.start(3 * 60 * 1_000)

        self._bg_refresh_t = QTimer(self)
        self._bg_refresh_t.timeout.connect(self._fetch_all_accounts)
        self._bg_refresh_t.start(60 * 1_000)

        self._incident_t = QTimer(self)
        self._incident_t.timeout.connect(self._fetch_incidents)
        self._incident_t.start(120_000)
        self._fetch_incidents()

    def _auto_refresh(self):
        active_file = self._account_manager.get_active_file()
        if active_file and not self._logging_in:
            self._fetch()

    # ---- incidents -----------------------------------------------

    def _fetch_incidents(self):
        self._incident_thread = IncidentFetchThread(self)
        self._incident_thread.result.connect(self._on_incidents)
        self._incident_thread.error.connect(lambda _: None)
        self._incident_thread.start()

    def _on_incidents(self, incidents_list):
        new_ids = {inc["id"] for inc in incidents_list}
        old_ids = set(self._known_incidents.keys())
        if not self._first_incident_fetch:
            for inc in incidents_list:
                if inc["id"] in new_ids - old_ids:
                    self._show_toast("new", inc["name"])
            for oid in old_ids - new_ids:
                self._show_toast("resolved", self._known_incidents[oid]["name"])
        self._first_incident_fetch = False
        self._known_incidents = {inc["id"]: inc for inc in incidents_list}
        self._rebuild_incidents(incidents_list)

    def _show_toast(self, kind, name):
        toast = _ToastNotification(kind, name, on_close=self._on_toast_closed)
        self._toasts.append(toast)
        self._reposition_toasts()
        toast.show()

    def _on_toast_closed(self, toast):
        if toast in self._toasts:
            self._toasts.remove(toast)
        self._reposition_toasts()

    def _reposition_toasts(self):
        screen_geo = QApplication.primaryScreen().availableGeometry()
        for i, toast in enumerate(self._toasts):
            toast.move(
                screen_geo.right() - toast.width() - 16,
                screen_geo.bottom() - (i + 1) * (toast.height() + 8),
            )

    def _rebuild_incidents(self, incidents):
        while self._incidents_layout.count():
            item = self._incidents_layout.takeAt(0)
            if item.widget():
                item.widget().deleteLater()
        if not incidents:
            self._incidents_w.hide()
            return
        for inc in incidents:
            lbl = _IncidentLabel(inc)
            self._incidents_layout.addWidget(lbl)
        self._incidents_w.show()
        self.adjustSize()

    # ---- compact mode --------------------------------------------

    def _enter_compact(self):
        self._compact_mode = True
        self._just_compacted = True
        self._header_w.hide()
        self._body_w.hide()
        self._incidents_w.hide()
        self._accounts_w.hide()
        self._update_compact_lbl()
        self._compact_w.show()
        # Remove fixed minimum width so window shrinks to fit compact label + buttons
        self._card.setMinimumWidth(0)
        self._save_state()
        self.adjustSize()

    def _exit_compact(self):
        self._compact_mode = False
        self._compact_w.hide()
        self._header_w.show()
        self._body_w.show()
        self._accounts_w.show()
        if self._known_incidents:
            self._incidents_w.show()
        # Restore minimum width for the full accounts table
        self._card.setMinimumWidth(
            _COL_CHK + 4 + _COL_NAME + _COL_PLAN + _COL_SESS + _COL_WEEK + 30
        )
        self.adjustSize()
        self._save_state()

    def _update_compact_lbl(self):
        m = next(
            (x for x in self._models if x["name"] == "Сессия"),
            self._models[0] if self._models else None,
        )
        if not m:
            self._compact_lbl.setText("\u2014")
            return
        pct = m["pct"]
        t = _fmt_remaining(m.get("reset_dt"))
        color = _pct_color(pct)
        self._compact_lbl.setText(f"{pct:.0f}%  ({t})" if t else f"{pct:.0f}%")
        self._compact_lbl.setStyleSheet(
            f"color:{color};font-size:11px;font-weight:600;"
        )

    # ---- login (CookieBridge flow) --------------------------------

    def _show_login(self):
        self._clear()
        lbl = QLabel("Открой claude.ai в браузере —\nCookieBridge захватит куки автоматически.")
        lbl.setStyleSheet("color:#888;font-size:10px;")
        lbl.setAlignment(Qt.AlignCenter)
        lbl.setWordWrap(True)
        self._body.addWidget(lbl)
        self._status_lbl.setText("")
        self.adjustSize()

    def _start_login(self):
        """Open browser, wait for CookieBridge to push cookies."""
        self._waiting_for_bridge = True
        self._clear()
        lbl = QLabel("Войди в аккаунт в браузере.\nРасширение CookieBridge захватит куки автоматически.")
        lbl.setStyleSheet(_SS_DIM)
        lbl.setWordWrap(True)
        lbl.setAlignment(Qt.AlignCenter)
        self._body.addWidget(lbl)
        self._status_lbl.setText("ожидание\u2026")
        self.adjustSize()

        webbrowser.open("https://claude.ai")

        # Timeout after 3 minutes
        if self._bridge_timeout_t:
            self._bridge_timeout_t.stop()
        self._bridge_timeout_t = QTimer(self)
        self._bridge_timeout_t.setSingleShot(True)
        self._bridge_timeout_t.timeout.connect(self._on_bridge_timeout)
        self._bridge_timeout_t.start(180_000)

    def _on_bridge_timeout(self):
        if not self._waiting_for_bridge:
            return
        self._waiting_for_bridge = False
        self._clear()
        lbl = QLabel("Куки не получены.\nУбедись, что расширение CookieBridge установлено и активно.")
        lbl.setStyleSheet("color:#f87171;font-size:10px;")
        lbl.setWordWrap(True)
        lbl.setAlignment(Qt.AlignCenter)
        self._body.addWidget(lbl)
        btn = QPushButton("Назад")
        btn.clicked.connect(self._show_login)
        self._body.addWidget(btn)
        self._status_lbl.setText("")
        self.adjustSize()

    def _add_account_playwright(self):
        """Add account: pick Chrome profile → open claude.ai/login → wait for CookieBridge."""
        if self._waiting_for_bridge:
            return
        profiles = _get_chrome_profiles()
        if len(profiles) > 1:
            self._show_chrome_profile_picker(profiles)
        else:
            profile_dir = profiles[0]["dir"] if profiles else "Default"
            self._open_chrome_for_login(profile_dir)

    def _show_chrome_profile_picker(self, profiles: list):
        """Show inline profile picker in accounts section."""
        # Replace bridge_status_lbl with a popup-style picker
        self._btn_add_acc.setEnabled(False)
        self._btn_add_acc.setText("выбери профиль↓")

        # Remove old picker if any
        if hasattr(self, "_profile_picker_w") and self._profile_picker_w:
            self._profile_picker_w.setParent(None)

        picker = QWidget()
        picker.setStyleSheet(
            "QWidget { background:#1e1e3a; border:1px solid #3a3a6a; border-radius:6px; padding:4px; }"
            "QPushButton { background:#2a2a4a; color:#c8c8e8; border:none; border-radius:4px;"
            "              font-size:10px; padding:4px 8px; text-align:left; }"
            "QPushButton:hover { background:#3a3a6a; }"
        )
        vb = QVBoxLayout(picker)
        vb.setContentsMargins(4, 4, 4, 4)
        vb.setSpacing(2)

        lbl = QLabel("Выбери профиль Chrome:")
        lbl.setStyleSheet("color:#8888aa; font-size:9px; background:transparent; border:none;")
        vb.addWidget(lbl)

        for p in profiles:
            line = p["name"]
            if p["email"]:
                line += f"  {p['email']}"
            btn = QPushButton(line)
            btn.setCursor(QCursor(Qt.PointingHandCursor))
            dir_name = p["dir"]
            btn.clicked.connect(lambda _, d=dir_name: self._on_profile_picked(d))
            vb.addWidget(btn)

        cancel = QPushButton("✕ Отмена")
        cancel.setStyleSheet("color:#f87171 !important;")
        cancel.clicked.connect(self._cancel_profile_picker)
        vb.addWidget(cancel)

        self._profile_picker_w = picker
        # Insert picker after bridge_status_lbl in acc_vbox
        parent_layout = self._bridge_status_lbl.parent().layout()
        idx = parent_layout.indexOf(self._bridge_status_lbl)
        parent_layout.insertWidget(idx + 1, picker)
        self.adjustSize()

    def _on_profile_picked(self, profile_dir: str):
        """User selected a Chrome profile."""
        if hasattr(self, "_profile_picker_w") and self._profile_picker_w:
            self._profile_picker_w.setParent(None)
            self._profile_picker_w.deleteLater()
            self._profile_picker_w = None
        self._open_chrome_for_login(profile_dir)

    def _cancel_profile_picker(self):
        if hasattr(self, "_profile_picker_w") and self._profile_picker_w:
            self._profile_picker_w.setParent(None)
            self._profile_picker_w.deleteLater()
            self._profile_picker_w = None
        self._btn_add_acc.setEnabled(True)
        self._btn_add_acc.setText("+ Добавить")
        # Force layout recalc so adjustSize can shrink the window
        self.layout().invalidate()
        self.layout().activate()
        QApplication.processEvents()
        self.resize(self.minimumSizeHint())
        self.adjustSize()

    def _open_chrome_for_login(self, profile_dir: str = "Default"):
        """Open Chrome with specified profile at claude.ai/login, wait for CookieBridge."""
        self._waiting_for_bridge = True
        self._btn_add_acc.setEnabled(False)
        self._btn_add_acc.setText("ожидание…")
        self._bridge_status_lbl.setText("Войди в аккаунт в открытом браузере…")
        self._bridge_status_lbl.show()
        self.adjustSize()

        chrome = _find_chrome()
        if chrome:
            subprocess.Popen(
                [chrome, f"--profile-directory={profile_dir}",
                 "--new-window", "https://claude.ai/login"],
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
        else:
            webbrowser.open("https://claude.ai/login")

        # Timeout after 3 minutes
        if self._bridge_timeout_t:
            self._bridge_timeout_t.stop()
        self._bridge_timeout_t = QTimer(self)
        self._bridge_timeout_t.setSingleShot(True)
        self._bridge_timeout_t.timeout.connect(self._on_add_account_timeout)
        self._bridge_timeout_t.start(180_000)

    def _start_playwright_login(self):
        """Re-login for the active account via Playwright."""
        if self._logging_in:
            return
        self._logging_in = True
        self._waiting_for_bridge = False
        self._clear()
        lbl = QLabel("Chrome открыт, войди в аккаунт\u2026")
        lbl.setStyleSheet(_SS_DIM)
        lbl.setWordWrap(True)
        lbl.setAlignment(Qt.AlignCenter)
        self._body.addWidget(lbl)
        self._status_lbl.setText("вход\u2026")
        self.adjustSize()

        tmp_file = ACCOUNTS_DIR / "tmp_pw_login.json"
        self._login_thread = LoginThread(tmp_file, self)
        self._login_thread.done.connect(lambda: self._on_playwright_done(tmp_file))
        self._login_thread.failed.connect(self._on_login_failed)
        self._login_thread.start()

    def _on_playwright_done(self, tmp_file: Path):
        self._logging_in = False
        self._btn_add_acc.setEnabled(True)
        self._btn_add_acc.setText("+ Добавить")
        self._bridge_status_lbl.hide()
        try:
            data = json.loads(tmp_file.read_text(encoding="utf-8"))
            cookies = data.get("cookies", [])
            if cookies:
                aid, is_new = self._account_manager.save_cookies(cookies)
                if aid and not self._account_manager.get_active_id():
                    self._account_manager.switch_to(aid)
                if aid:
                    # Try to associate current Claude Code CLI tokens
                    self._try_associate_cc_tokens(aid)
                    self._fetch_identity(aid)
        except Exception:
            pass
        finally:
            tmp_file.unlink(missing_ok=True)
        self._fetch()
        self._schedule_rebuild_accounts()

    def _on_login_failed(self, msg):
        self._logging_in = False
        self._btn_add_acc.setEnabled(True)
        self._btn_add_acc.setText("+ Добавить")
        self._bridge_status_lbl.hide()
        self._clear()
        lbl = QLabel(f"Ошибка: {msg[:120]}")
        lbl.setStyleSheet("color:#f87171;font-size:9px;")
        lbl.setWordWrap(True)
        self._body.addWidget(lbl)
        btn = QPushButton("Попробовать снова")
        btn.clicked.connect(self._show_login)
        self._body.addWidget(btn)
        self.adjustSize()

    # ---- CookieBridge cookie handler -----------------------------

    def on_bridge_cookies(self, cookies: list):
        """Called when CookieBridge extension pushes claude.ai cookies."""
        aid, is_new = self._account_manager.save_cookies(cookies)
        if not aid:
            return

        if self._bridge_timeout_t:
            self._bridge_timeout_t.stop()
            self._bridge_timeout_t = None

        if self._waiting_for_bridge:
            self._waiting_for_bridge = False
            # Re-enable "+ Добавить" button if it was waiting
            if hasattr(self, "_btn_add_acc"):
                self._btn_add_acc.setEnabled(True)
                self._btn_add_acc.setText("+ Добавить")
            self._bridge_status_lbl.hide()
            if not self._account_manager.get_active_id():
                self._account_manager.switch_to(aid)
            self._clear()
            self._status_lbl.setText("")

        if is_new:
            # Brand new account — fetch identity to detect duplicates by email
            if not self._account_manager.get_active_id():
                self._account_manager.switch_to(aid)
            self._fetch_identity(aid)
        else:
            # Known account — cookies refreshed, clear error
            self._accounts_errors.pop(aid, None)
            # If email/plan still missing (e.g. prev fetch failed), retry now
            acc_meta = next((a for a in self._account_manager.get_all_including_pending()
                             if a["id"] == aid), None)
            if acc_meta and (not acc_meta.get("email") or not acc_meta.get("plan")):
                self._fetch_identity(aid)
            elif aid != self._account_manager.get_active_id():
                QTimer.singleShot(500, lambda: self._fetch_one_bg(aid))

        self._fetch()
        self._schedule_rebuild_accounts()

    def _on_add_account_timeout(self):
        self._waiting_for_bridge = False
        if hasattr(self, "_btn_add_acc"):
            self._btn_add_acc.setEnabled(True)
            self._btn_add_acc.setText("+ Добавить")
        self._bridge_status_lbl.setText("Куки не получены. Расширение CookieBridge установлено?")
        QTimer.singleShot(4000, lambda: (
            self._bridge_status_lbl.hide(),
            self.adjustSize(),
        ))
        self.adjustSize()

    # ---- Identity fetch ------------------------------------------

    def _fetch_identity(self, account_id: str):
        acc_file = self._account_manager.get_account_file(account_id)
        if not acc_file.exists():
            return
        t = IdentityThread(acc_file, account_id, self)
        t.result.connect(self._on_identity)   # (account_id, email, name, plan, uuid)
        t.finished.connect(lambda: self._on_identity_thread_done(account_id, t))
        t.start()
        self._identity_threads.append(t)
        self._identity_threads = [x for x in self._identity_threads if x.isRunning()]

    def _try_associate_cc_tokens(self, account_id: str):
        """If ~/.claude/.credentials.json has tokens not yet associated, bind to account_id."""
        try:
            if not CLAUDE_CODE_CREDS.exists():
                return
            creds = json.loads(CLAUDE_CODE_CREDS.read_text(encoding="utf-8"))
            oauth = creds.get("claudeAiOauth", {})
            token = oauth.get("accessToken", "")
            if not token:
                return
            # Already associated with someone?
            for acc in self._account_manager.get_all_including_pending():
                if acc.get("cc_tokens", {}).get("accessToken") == token:
                    return
            self._account_manager.update_cc_tokens(account_id, oauth)
        except Exception:
            pass

    def _apply_cc_tokens(self, account_id: str):
        """Write Claude Code tokens for this account to ~/.claude/.credentials.json"""
        tokens = self._account_manager.get_cc_tokens(account_id)
        if not tokens or not tokens.get("accessToken"):
            return
        try:
            creds = {"claudeAiOauth": tokens}
            CLAUDE_CODE_CREDS.write_text(
                json.dumps(creds, indent=2), encoding="utf-8"
            )
        except Exception:
            pass

    def _on_identity_thread_done(self, account_id: str, thread):
        """Fallback: if identity fetch failed/timed out, confirm account anyway after 30s."""
        # If account is still pending (identity didn't emit result), confirm it
        all_acc = self._account_manager.get_all_including_pending()
        acc = next((a for a in all_acc if a["id"] == account_id), None)
        if acc and acc.get("pending"):
            QTimer.singleShot(30_000, lambda: self._confirm_pending_fallback(account_id))

    def _confirm_pending_fallback(self, account_id: str):
        """After 30s timeout, confirm pending account only if it has email (not a ghost)."""
        all_acc = self._account_manager.get_all_including_pending()
        acc = next((a for a in all_acc if a["id"] == account_id), None)
        if acc and acc.get("pending"):
            if acc.get("email"):
                # Has email — safe to confirm
                self._account_manager.confirm_account(account_id)
                self._schedule_rebuild_accounts()
            else:
                # No email yet — retry identity fetch instead of confirming a ghost
                self._fetch_identity(account_id)
                # Try again after another 60s
                QTimer.singleShot(60_000, lambda: self._confirm_pending_final(account_id))

    def _confirm_pending_final(self, account_id: str):
        """Last resort: confirm or remove ghost after 90s total."""
        all_acc = self._account_manager.get_all_including_pending()
        acc = next((a for a in all_acc if a["id"] == account_id), None)
        if acc and acc.get("pending"):
            if acc.get("email"):
                self._account_manager.confirm_account(account_id)
            else:
                # Ghost account with no email after 90s — remove it
                self._account_manager.remove(account_id)
            self._schedule_rebuild_accounts()

    def _on_identity(self, account_id: str, email: str, name: str, plan: str, uuid: str = ""):
        # Dedup: first try by Anthropic UUID (most reliable), then by email
        dup_id = ""
        if uuid:
            found = self._account_manager.find_by_uuid(uuid)
            if found and found != account_id:
                dup_id = found
        if not dup_id and email:
            existing = next(
                (a for a in self._account_manager.get_all_including_pending()
                 if a["id"] != account_id and a.get("email") == email),
                None,
            )
            if existing:
                dup_id = existing["id"]

        if dup_id:
            eid = dup_id
            new_file = self._account_manager.get_account_file(account_id)
            existing_file = self._account_manager.get_account_file(eid)
            # Update existing account's cookies with fresh ones
            if new_file.exists():
                existing_file.write_bytes(new_file.read_bytes())
            # If pending account was set as active → switch to real one
            if self._account_manager.get_active_id() == account_id:
                self._account_manager.switch_to(eid)
            if account_id in self._accounts_data:
                self._accounts_data[eid] = self._accounts_data.pop(account_id)
            self._accounts_errors.pop(account_id, None)
            self._accounts_errors.pop(eid, None)
            self._account_manager.remove(account_id)
            self._account_manager.update_info(eid, email=email, name=name, plan=plan, uuid=uuid)
            self._account_manager.confirm_account(eid)
            # Re-associate CC tokens to the canonical account
            self._try_associate_cc_tokens(eid)
            QTimer.singleShot(200, self._fetch)
            self._schedule_rebuild_accounts()
            return

        # Genuinely new account — confirm it so it appears in the list
        self._account_manager.confirm_account(account_id)
        self._account_manager.update_info(account_id, email=email, name=name, plan=plan, uuid=uuid)
        self._try_associate_cc_tokens(account_id)
        self._schedule_rebuild_accounts()

    # ---- Refresh identities for accounts missing email/plan ------

    def _refresh_missing_identities(self):
        for i, acc in enumerate(self._account_manager.get_all_including_pending()):
            if not acc.get("email") or not acc.get("plan") or acc.get("pending"):
                QTimer.singleShot(i * 5000, lambda aid=acc["id"]: self._fetch_identity(aid))

    def _associate_cc_tokens_on_startup(self):
        """On startup, associate current CC tokens + start watching .credentials.json."""
        self._cc_creds_watcher = QFileSystemWatcher(self)
        creds_path = str(CLAUDE_CODE_CREDS)
        if CLAUDE_CODE_CREDS.exists():
            self._cc_creds_watcher.addPath(creds_path)
        # Also watch the parent dir (file may be recreated)
        creds_dir = str(CLAUDE_CODE_CREDS.parent)
        self._cc_creds_watcher.addPath(creds_dir)
        self._cc_creds_watcher.fileChanged.connect(self._on_cc_creds_changed)
        self._cc_creds_watcher.directoryChanged.connect(self._on_cc_creds_dir_changed)
        self._cc_last_token = ""
        # Associate current tokens
        self._on_cc_creds_changed(creds_path)

    def _on_cc_creds_dir_changed(self, _path):
        """Re-add file watch if .credentials.json was recreated."""
        if CLAUDE_CODE_CREDS.exists():
            self._cc_creds_watcher.addPath(str(CLAUDE_CODE_CREDS))
            self._on_cc_creds_changed(str(CLAUDE_CODE_CREDS))

    def _on_cc_creds_changed(self, _path=None):
        """Called when ~/.claude/.credentials.json changes. Identify owner by UUID."""
        try:
            if not CLAUDE_CODE_CREDS.exists():
                return
            creds = json.loads(CLAUDE_CODE_CREDS.read_text(encoding="utf-8"))
            oauth = creds.get("claudeAiOauth", {})
            token = oauth.get("accessToken", "")
            if not token or token == self._cc_last_token:
                return
            self._cc_last_token = token
            # Check if already associated
            for acc in self._account_manager.get_all_including_pending():
                if acc.get("cc_tokens", {}).get("accessToken") == token:
                    return
            # Associate with currently active account — user does /login right after switching
            active_id = self._account_manager.get_active_id()
            if active_id:
                self._account_manager.update_cc_tokens(active_id, oauth)
        except Exception:
            pass

    # ---- Remove account ------------------------------------------

    def _remove_account(self, account_id: str):
        self._account_manager.remove(account_id)
        self._accounts_data.pop(account_id, None)
        self._accounts_errors.pop(account_id, None)
        self._fetching_ids.discard(account_id)
        self._schedule_rebuild_accounts()

    # ---- Switch account ------------------------------------------

    def _switch_account(self, account_id: str):
        self._account_manager.switch_to(account_id)
        # Write Claude Code CLI credentials if we have tokens for this account
        self._apply_cc_tokens(account_id)
        # Show cached data immediately if available — no flicker
        cached = self._accounts_data.get(account_id)
        if cached:
            self._rebuild(cached)
        else:
            self._models = []
            self._rows = []
        self._schedule_rebuild_accounts()
        # Refresh in background — error won't clear the screen
        self._fetch(silent=True)

    # ---- Fetch active account ------------------------------------

    def _fetch(self, silent: bool = False):
        auth = self._account_manager.get_active_file()
        if not auth or not auth.exists():
            self._show_login()
            return
        active_id = self._account_manager.get_active_id()
        if active_id in self._fetching_ids:
            return  # already fetching this account
        self._fetching_ids.add(active_id)
        self._status_lbl.setText("")
        t = FetchThread(auth, active_id, self)
        t.result.connect(lambda aid, m: self._on_result(m, aid))
        t.error.connect(lambda aid, msg: self._on_error(msg, silent=silent, account_id=aid))
        t.session_expired.connect(lambda aid: self._on_session_expired(aid))
        t.start()
        self._thread = t

    # ---- Fetch all (background) ----------------------------------

    def _fetch_all_accounts(self):
        """Fetch non-active accounts one-by-one with 3s gap to avoid curl timeouts."""
        active_id = self._account_manager.get_active_id()
        queue = [
            acc["id"] for acc in self._account_manager.get_all()
            if acc["id"] != active_id
            and self._account_manager.get_account_file(acc["id"]).exists()
            and acc["id"] not in self._fetching_ids
        ]
        for i, aid in enumerate(queue):
            QTimer.singleShot(i * 3000, lambda _aid=aid: self._fetch_one_bg(_aid))

    def _fetch_one_bg(self, account_id: str):
        """Fetch a single background account (no-op if already in progress)."""
        if account_id in self._fetching_ids:
            return
        acc_file = self._account_manager.get_account_file(account_id)
        if not acc_file.exists():
            return
        self._fetching_ids.add(account_id)
        t = FetchThread(acc_file, account_id, self)
        t.result.connect(self._on_bg_result)
        t.error.connect(self._on_bg_error)
        t.session_expired.connect(self._on_bg_expired)
        t.start()
        self._bg_threads = [x for x in self._bg_threads if x.isRunning()]
        self._bg_threads.append(t)

    def _on_bg_result(self, account_id: str, models: list):
        self._fetching_ids.discard(account_id)
        self._accounts_data[account_id] = models
        self._accounts_errors.pop(account_id, None)
        self._schedule_rebuild_accounts()
        self._check_autopin_candidates()

    def _on_bg_error(self, account_id: str, msg: str):
        self._fetching_ids.discard(account_id)
        # curl 28 = timeout, show separately
        self._accounts_errors[account_id] = "timeout" if "28" in msg else "err"
        self._schedule_rebuild_accounts()

    def _on_bg_expired(self, account_id: str):
        self._fetching_ids.discard(account_id)
        self._accounts_errors[account_id] = "exp"
        self._schedule_rebuild_accounts()

    def _schedule_rebuild_accounts(self):
        """Debounce: rebuild at most once per 400ms to prevent flicker."""
        if self._rebuild_debounce_t is None:
            self._rebuild_debounce_t = QTimer(self)
            self._rebuild_debounce_t.setSingleShot(True)
            self._rebuild_debounce_t.timeout.connect(self._do_rebuild_accounts)
        self._rebuild_debounce_t.start(400)

    # ---- Auto-kickstart ------------------------------------------

    def _check_autopin_candidates(self):
        """After any data refresh, queue accounts with 0% session for auto-ping."""
        now = datetime.now()
        for acc in self._account_manager.get_all():
            aid = acc["id"]
            models = self._accounts_data.get(aid, [])
            session_m = next((m for m in models if m["name"] == "Сессия"), None)
            if session_m is None:
                continue
            # Only kick accounts sitting at exactly 0%
            if session_m["pct"] > 0:
                continue
            # Skip if weekly limit is exhausted — Claude won't respond anyway
            weekly_m = next((m for m in models if m["name"] == "Все модели"), None)
            if weekly_m and weekly_m["pct"] >= 100:
                continue
            # Don't re-ping if already in queue
            if aid in self._autopin_queue:
                continue
            # Don't re-ping same account within 4 hours
            last = self._autopin_cooldown.get(aid)
            if last and (now - last).total_seconds() < 4 * 3600:
                continue
            self._autopin_queue.append(aid)

        if self._autopin_queue and not self._autopin_scheduled:
            self._schedule_next_autopin()

    def _schedule_next_autopin(self):
        """Schedule the next queued ping, respecting ≥1h gap between pings."""
        if not self._autopin_queue:
            self._autopin_scheduled = False
            return
        self._autopin_scheduled = True
        now = datetime.now()
        delay_ms = 0
        if self._autopin_last_sent:
            elapsed = (now - self._autopin_last_sent).total_seconds()
            if elapsed < 3600:
                delay_ms = int((3600 - elapsed) * 1000)
        QTimer.singleShot(delay_ms, self._fire_next_autopin)

    def _fire_next_autopin(self):
        if not self._autopin_queue:
            self._autopin_scheduled = False
            return
        aid = self._autopin_queue.pop(0)
        acc_file = self._account_manager.get_account_file(aid)
        if not acc_file.exists():
            self._schedule_next_autopin()
            return
        # Re-check: skip if session is no longer 0% (user may have sent a message)
        models = self._accounts_data.get(aid, [])
        session_m = next((m for m in models if m["name"] == "Сессия"), None)
        if session_m and session_m["pct"] > 0:
            self._schedule_next_autopin()
            return
        self._autopin_last_sent = datetime.now()
        self._autopin_cooldown[aid] = datetime.now()
        t = PingThread(acc_file, self)
        t.done.connect(lambda: self._on_autopin_done(aid))
        t.error.connect(lambda e, _aid=aid: self._on_autopin_error(_aid, e))
        t.finished.connect(lambda: self._autopin_threads.remove(t) if t in self._autopin_threads else None)
        t.start()
        self._autopin_threads.append(t)
        # Schedule the next one (will wait ≥1h from now)
        self._autopin_scheduled = False
        self._schedule_next_autopin()

    def _on_autopin_done(self, account_id: str):
        # Refresh data for this account so UI shows updated session %
        QTimer.singleShot(5000, lambda: self._fetch_one_bg(account_id))

    def _on_autopin_error(self, account_id: str, err: str):
        # Remove cooldown so it can retry next data refresh
        self._autopin_cooldown.pop(account_id, None)

    # ---- Result handlers -----------------------------------------

    def _on_result(self, models, account_id: str = ""):
        self._fetching_ids.discard(account_id)
        active_id = self._account_manager.get_active_id()
        # Only update the main view if the result is for the currently active account
        if account_id and account_id != active_id:
            self._accounts_data[account_id] = models
            self._schedule_rebuild_accounts()
            self._check_autopin_candidates()
            return
        if active_id:
            self._accounts_data[active_id] = models
        self._rebuild(models)
        self._schedule_rebuild_accounts()
        self._check_autopin_candidates()
        self._status_lbl.setText("")

    def _on_error(self, msg: str, silent: bool = False, account_id: str = ""):
        self._fetching_ids.discard(account_id)
        if silent:
            # Don't touch the main body — just mark error in the accounts table
            if account_id:
                self._accounts_errors[account_id] = "timeout" if "28" in msg else "err"
            self._status_lbl.setText("!")
            self._schedule_rebuild_accounts()
            return
        self._status_lbl.setText("!")
        # Show error only if there's nothing to display
        if not self._models:
            self._clear()
            lbl = QLabel(msg[:180])
            lbl.setStyleSheet("color:#f87171;font-size:9px;")
            lbl.setWordWrap(True)
            self._body.addWidget(lbl)
            self.adjustSize()

    def _on_session_expired(self, account_id: str):
        self._fetching_ids.discard(account_id)
        active_id = self._account_manager.get_active_id()
        if account_id == active_id:
            self._show_login()

    # ---- Rebuild model rows (active account) ---------------------

    def _clear(self):
        self._rows.clear()
        while self._body.count():
            w = self._body.takeAt(0).widget()
            if w:
                w.deleteLater()

    def _rebuild(self, models):
        self._models = models
        self._clear()
        if not models:
            lbl = QLabel("Нет данных — нажми \u27f3")
            lbl.setStyleSheet(_SS_DIM)
            lbl.setAlignment(Qt.AlignCenter)
            self._body.addWidget(lbl)
        else:
            # Show only the two key metrics in the top section
            _TOP_KEYS = {"Сессия", "Все модели"}
            for m in models:
                if m["name"] not in _TOP_KEYS:
                    continue
                countdown = _fmt_remaining(m.get("reset_dt"))
                row = _ModelRow(m["name"], m["pct"], m.get("reset_dt"), countdown)
                self._rows.append(row)
                self._body.addWidget(row)
            self._update_tray(models)
        if self._compact_mode:
            self._update_compact_lbl()
        self.adjustSize()

    # ---- Rebuild accounts table ----------------------------------

    def _do_rebuild_accounts(self):
        while self._acc_rows_layout.count():
            w = self._acc_rows_layout.takeAt(0).widget()
            if w:
                w.deleteLater()

        accounts = self._account_manager.get_all()
        active_id = self._account_manager.get_active_id()

        if not accounts:
            self.adjustSize()
            return

        def _sort_key(acc):
            aid = acc["id"]
            models = self._accounts_data.get(aid, [])
            session_m = next((m for m in models if m["name"] == "Сессия"), None)
            weekly_m  = next((m for m in models if m["name"] == "Все модели"), None)
            weekly_pct  = weekly_m["pct"]  if weekly_m  else 0
            session_pct = session_m["pct"] if session_m else 0
            if session_m and session_m.get("reset_dt"):
                secs = max(0, int((session_m["reset_dt"] - datetime.now()).total_seconds()))
            else:
                secs = 99999
            # Group 3: weekly exhausted — unusable until weekly resets
            if weekly_pct >= 100:
                return (3, 99999, 0)
            # Group 2: session full — can't send messages right now, wait for reset
            if session_pct >= 100:
                return (2, secs, 0)
            # Group 1: usable — soonest 5h reset first; tiebreak: higher usage first
            # (already used more = more urgent to keep spending before the window closes)
            return (1, secs, -session_pct)

        accounts = sorted(accounts, key=_sort_key)

        for acc in accounts:
            aid = acc["id"]
            is_active = aid == active_id
            display = acc.get("email") or acc.get("name") or ""
            plan    = acc.get("plan", "")
            err     = self._accounts_errors.get(aid) if not is_active else None

            models = self._accounts_data.get(aid, [])
            session_m = next((m for m in models if m["name"] == "Сессия"), None)
            weekly_m  = next((m for m in models if m["name"] == "Все модели"), None)

            session_pct      = session_m["pct"] if session_m else None
            session_reset_dt = session_m["reset_dt"] if session_m else None
            session_time     = _fmt_remaining(session_reset_dt)
            weekly_pct       = weekly_m["pct"] if weekly_m else None
            weekly_reset_dt  = weekly_m["reset_dt"] if weekly_m else None
            weekly_time      = _fmt_remaining(weekly_reset_dt)

            row = _AccountRow(
                aid, display, is_active,
                plan=plan,
                session_pct=session_pct, session_time=session_time,
                weekly_pct=weekly_pct,   weekly_time=weekly_time,
                error=err,
            )
            row._session_reset_dt = session_reset_dt
            row._weekly_reset_dt  = weekly_reset_dt
            row.switch_requested.connect(self._switch_account)
            row.remove_requested.connect(self._remove_account)
            self._acc_rows_layout.addWidget(row)

        # After bridge cookies arrived, hide status label
        if not self._waiting_for_bridge:
            self._bridge_status_lbl.hide()

        self.adjustSize()

    # ---- Tray update ---------------------------------------------

    def _update_tray(self, models):
        lines = []
        for m in models:
            line = f"{m['name']}: {m['pct']:.0f}%"
            reset_dt = m.get("reset_dt")
            if reset_dt:
                secs = int((reset_dt - datetime.now()).total_seconds())
                if secs > 0:
                    if secs >= 86400:
                        d = secs // 86400
                        h = (secs % 86400) // 3600
                        mm = (secs % 3600) // 60
                        line += f"  {d}:{h:02d}:{mm:02d}"
                    else:
                        h = secs // 3600
                        mm = (secs % 3600) // 60
                        line += f"  {h}:{mm:02d}"
            lines.append(line)
        self._tray.setToolTip("Claude Usage\n" + "\n".join(lines))
        session = next((m for m in models if m["name"] == "Сессия"), models[0] if models else None)
        if session:
            self._tray.setIcon(_make_pct_icon(session["pct"]))

    # ---- Countdown tick ------------------------------------------

    def _tick(self):
        need_refresh = False
        for row in self._rows:
            if row.reset_dt is None:
                continue
            secs = int((row.reset_dt - datetime.now()).total_seconds())
            if secs <= 0:
                need_refresh = True
            else:
                row.update_timer(_fmt_remaining(row.reset_dt))
        # Tick countdowns for background account rows
        for i in range(self._acc_rows_layout.count()):
            w = self._acc_rows_layout.itemAt(i).widget()
            if isinstance(w, _AccountRow):
                w.tick_timers()
        if self._compact_mode:
            self._update_compact_lbl()
        if need_refresh:
            self._fetch()

    # ---- Window state persistence --------------------------------

    def _load_state(self):
        try:
            data = json.loads(_STATE_FILE.read_text(encoding="utf-8"))
            self.move(int(data["x"]), int(data["y"]))
            if data.get("compact"):
                self._enter_compact()
        except (FileNotFoundError, KeyError, ValueError, json.JSONDecodeError):
            self.move(40, 40)

    def _save_state(self):
        data = {
            "x": self.x(),
            "y": self.y(),
            "compact": self._compact_mode,
        }
        try:
            _STATE_FILE.write_text(json.dumps(data), encoding="utf-8")
        except OSError:
            pass

    # ---- Tray ----------------------------------------------------

    def hide_to_tray(self):
        self.hide()

    # ---- Drag ----------------------------------------------------

    def mousePressEvent(self, e):
        if e.button() == Qt.LeftButton:
            self._drag = e.globalPos() - self.frameGeometry().topLeft()
            self._drag_started = False

    def mouseMoveEvent(self, e):
        if e.buttons() == Qt.LeftButton and not self._drag.isNull():
            self._drag_started = True
            self.move(e.globalPos() - self._drag)

    def mouseReleaseEvent(self, e):
        if e.button() == Qt.LeftButton and self._compact_mode and not self._drag_started:
            if self._just_compacted:
                self._just_compacted = False
            else:
                self._exit_compact()
        if self._drag_started:
            self._save_state()
        self._drag = QPoint()
        self._drag_started = False

    def contextMenuEvent(self, e):
        menu = QMenu(self)
        menu.setStyleSheet(
            "QMenu{background:#1a1a1a;border:1px solid #333;color:#ccc;font-size:11px;}"
            "QMenu::item:selected{background:#333;}"
        )
        menu.addAction("Обновить", self._fetch)
        menu.addAction("Обновить все аккаунты", self._fetch_all_accounts)
        menu.addAction("Claude Status", lambda: webbrowser.open("https://status.claude.com"))
        menu.addAction("Свернуть в трей", self.hide_to_tray)
        menu.addSeparator()

        # Per-account switch submenu
        accounts = self._account_manager.get_all()
        active_id = self._account_manager.get_active_id()
        if len(accounts) > 1:
            acc_menu = menu.addMenu("Переключить аккаунт")
            acc_menu.setStyleSheet(
                "QMenu{background:#1a1a1a;border:1px solid #333;color:#ccc;font-size:11px;}"
                "QMenu::item:selected{background:#333;}"
            )
            for acc in accounts:
                aid = acc["id"]
                display = acc.get("email") or acc.get("name") or "Аккаунт"
                label = f"✓ {display}" if aid == active_id else f"  {display}"
                acc_menu.addAction(label, lambda _aid=aid: self._switch_account(_aid))

        menu.addSeparator()
        menu.addAction("Войти снова", self._re_login_browser)
        menu.addSeparator()
        menu.addAction("Выйти", QApplication.instance().quit)
        menu.exec_(e.globalPos())

    def _re_login_browser(self):
        self._show_login()

    def _re_login_playwright(self):
        self._start_playwright_login()

    def _re_login(self):
        self._show_login()


# ---------------------------------------------------------------------------
# Clickable widget (for header toggle)
# ---------------------------------------------------------------------------

class _ClickableWidget(QWidget):
    clicked = pyqtSignal()

    def __init__(self, parent=None):
        super().__init__(parent)
        self._press_pos = None

    def mousePressEvent(self, e):
        if e.button() == Qt.LeftButton:
            self._press_pos = e.globalPos()
        super().mousePressEvent(e)

    def mouseReleaseEvent(self, e):
        if e.button() == Qt.LeftButton and self._press_pos is not None:
            if (e.globalPos() - self._press_pos).manhattanLength() < 5:
                self.clicked.emit()
        self._press_pos = None
        super().mouseReleaseEvent(e)


# ---------------------------------------------------------------------------
# Card widget
# ---------------------------------------------------------------------------

class _Card(QWidget):
    def paintEvent(self, _):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        p.setBrush(QColor(12, 12, 12, 225))
        p.setPen(QColor(255, 255, 255, 18))
        from PyQt5.QtCore import QRectF
        p.drawRoundedRect(QRectF(self.rect()).adjusted(0.5, 0.5, -0.5, -0.5), 8, 8)


# ---------------------------------------------------------------------------
# Toast notification (new/resolved incident)
# ---------------------------------------------------------------------------

class _ToastNotification(QWidget):
    def __init__(self, kind: str, name: str, on_close=None):
        super().__init__()
        self._on_close = on_close
        self._drag = QPoint()
        self.setWindowFlags(Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool)
        self.setAttribute(Qt.WA_TranslucentBackground)
        self.setAttribute(Qt.WA_ShowWithoutActivating)
        self.setFixedWidth(360)

        layout = QHBoxLayout(self)
        layout.setContentsMargins(14, 10, 14, 10)
        layout.setSpacing(8)

        if kind == "new":
            icon_lbl = QLabel("!")
            icon_lbl.setStyleSheet("color:#f87171;font-size:14px;font-weight:bold;")
            msg = f"Инцидент: {name}"
        else:
            icon_lbl = QLabel("OK")
            icon_lbl.setStyleSheet("color:#4ade80;font-size:14px;font-weight:bold;")
            msg = f"Завершён: {name}"
        layout.addWidget(icon_lbl, 0)

        text_lbl = QLabel(msg)
        text_lbl.setStyleSheet("color:#e0e0e0;font-size:13px;")
        text_lbl.setWordWrap(True)
        layout.addWidget(text_lbl, 1)

        btn_x = QPushButton("\u00d7")
        btn_x.setFixedSize(18, 18)
        btn_x.setStyleSheet(_SS_BTN)
        btn_x.clicked.connect(self._close)
        layout.addWidget(btn_x, 0, Qt.AlignTop)

    def _close(self):
        if self._on_close:
            self._on_close(self)
        self.close()
        self.deleteLater()

    def mousePressEvent(self, e):
        if e.button() == Qt.LeftButton:
            self._drag = e.globalPos() - self.frameGeometry().topLeft()
        elif e.button() == Qt.RightButton:
            self._close()

    def mouseMoveEvent(self, e):
        if e.buttons() == Qt.LeftButton and not self._drag.isNull():
            self.move(e.globalPos() - self._drag)

    def mouseReleaseEvent(self, e):
        self._drag = QPoint()

    def paintEvent(self, _):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        p.setBrush(QColor(12, 12, 12, 225))
        p.setPen(QColor(255, 255, 255, 18))
        from PyQt5.QtCore import QRectF
        p.drawRoundedRect(QRectF(self.rect()).adjusted(0.5, 0.5, -0.5, -0.5), 8, 8)


# ---------------------------------------------------------------------------
# Incident popup (details)
# ---------------------------------------------------------------------------

class _IncidentPopup(QWidget):
    def __init__(self, incident_data: dict, parent=None):
        super().__init__(parent)
        self._drag = QPoint()
        self.setWindowFlags(Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool)
        self.setAttribute(Qt.WA_TranslucentBackground)
        self.setFixedWidth(340)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(14, 10, 14, 10)
        layout.setSpacing(6)

        header = QHBoxLayout()
        header.setSpacing(6)
        name_lbl = QLabel(incident_data.get("name", ""))
        name_lbl.setStyleSheet("color:#fff;font-size:14px;font-weight:bold;")
        name_lbl.setWordWrap(True)
        header.addWidget(name_lbl, 1)

        btn_close = QPushButton("\u00d7")
        btn_close.setFixedSize(18, 18)
        btn_close.setStyleSheet(_SS_BTN)
        btn_close.clicked.connect(self.close)
        header.addWidget(btn_close, 0, Qt.AlignTop)
        layout.addLayout(header)

        impact = incident_data.get("impact", "none")
        color = _IMPACT_COLORS.get(impact, _IMPACT_COLORS["none"])
        status_lbl = QLabel(incident_data.get("status", ""))
        status_lbl.setStyleSheet(f"color:{color};font-size:13px;")
        layout.addWidget(status_lbl)

        body = incident_data.get("last_update_body", "")
        if body:
            desc_lbl = QLabel(body)
            desc_lbl.setStyleSheet("color:#bbb;font-size:13px;")
            desc_lbl.setWordWrap(True)
            desc_lbl.setMaximumWidth(316)
            layout.addWidget(desc_lbl)

        shortlink = incident_data.get("shortlink", "")
        if shortlink:
            btn_more = QPushButton("Подробнее")
            btn_more.setCursor(QCursor(Qt.PointingHandCursor))
            btn_more.setStyleSheet(
                "QPushButton{background:#3b82f6;color:#fff;border:none;"
                "border-radius:5px;padding:5px 10px;font-size:13px;}"
                "QPushButton:hover{background:#2563eb;}"
            )
            btn_more.clicked.connect(lambda: webbrowser.open(shortlink))
            layout.addWidget(btn_more)

    def mousePressEvent(self, e):
        if e.button() == Qt.LeftButton:
            self._drag = e.globalPos() - self.frameGeometry().topLeft()
        elif e.button() == Qt.RightButton:
            self.close()

    def mouseMoveEvent(self, e):
        if e.buttons() == Qt.LeftButton and not self._drag.isNull():
            self.move(e.globalPos() - self._drag)

    def mouseReleaseEvent(self, e):
        self._drag = QPoint()

    def paintEvent(self, _):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        p.setBrush(QColor(12, 12, 12, 225))
        p.setPen(QColor(255, 255, 255, 18))
        from PyQt5.QtCore import QRectF
        p.drawRoundedRect(QRectF(self.rect()).adjusted(0.5, 0.5, -0.5, -0.5), 8, 8)


# ---------------------------------------------------------------------------
# Incident label: "● Elevated error rates"
# ---------------------------------------------------------------------------

_IMPACT_COLORS = {
    "critical":    "#f87171",
    "major":       "#fb923c",
    "minor":       "#facc15",
    "maintenance": "#60a5fa",
    "none":        "#4ade80",
}


class _IncidentLabel(QLabel):
    def __init__(self, incident_data: dict, parent=None):
        super().__init__(parent)
        self._data = incident_data
        self._popup = None
        impact = incident_data.get("impact", "none")
        color = _IMPACT_COLORS.get(impact, _IMPACT_COLORS["none"])
        name = incident_data.get("name", "")
        if len(name) > 45:
            name = name[:42] + "\u2026"
        self._color = color
        self.setText(f"\u25cf {name}")
        self.setStyleSheet(f"color:{color};font-size:11px;")
        self.setCursor(QCursor(Qt.PointingHandCursor))

    def enterEvent(self, event):
        self.setStyleSheet(f"color:{self._color};font-size:11px;text-decoration:underline;")
        super().enterEvent(event)

    def leaveEvent(self, event):
        self.setStyleSheet(f"color:{self._color};font-size:11px;")
        super().leaveEvent(event)

    def mousePressEvent(self, event):
        if event.button() == Qt.LeftButton:
            self._show_popup()
        else:
            super().mousePressEvent(event)

    def _show_popup(self):
        if self._popup:
            self._popup.close()
        self._popup = _IncidentPopup(self._data)
        global_pos = self.mapToGlobal(QPoint(0, 0))
        screen_geo = QApplication.primaryScreen().availableGeometry()
        if global_pos.y() > screen_geo.center().y():
            self._popup.move(global_pos.x(), global_pos.y() - self._popup.sizeHint().height())
        else:
            self._popup.move(global_pos.x(), global_pos.y() + self.height())
        self._popup.show()


# ---------------------------------------------------------------------------
# Model row: "Сессия       15%  (2ч 43м)"
# ---------------------------------------------------------------------------

class _ModelRow(QWidget):
    def __init__(self, name: str, pct: float, reset_dt, countdown: str):
        super().__init__()
        self.pct = pct
        self.reset_dt = reset_dt
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)

        name_lbl = QLabel(name)
        name_lbl.setStyleSheet("color:#e0e0e0;font-size:11px;")
        layout.addWidget(name_lbl)

        color = _pct_color(pct)
        self._pct_lbl = QLabel(f" {pct:.0f}%")
        self._pct_lbl.setStyleSheet(f"color:{color};font-size:11px;font-weight:600;")
        layout.addWidget(self._pct_lbl)

        self._timer_lbl = QLabel(f"  ({countdown})" if countdown else "")
        self._timer_lbl.setStyleSheet("color:#aaa;font-size:10px;")
        layout.addWidget(self._timer_lbl)

    def update_timer(self, t: str):
        self._timer_lbl.setText(f"  ({t})" if t else "")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def _kill_previous_instances():
    my_pid = os.getpid()
    try:
        my_ppid = psutil.Process(my_pid).ppid()
    except psutil.NoSuchProcess:
        my_ppid = -1
    my_name = "ClaudeMonitor.exe"
    script_name = "usage_monitor.py"
    skip_pids = {my_pid, my_ppid}

    for proc in psutil.process_iter(["pid", "name", "cmdline"]):
        if proc.info["pid"] in skip_pids:
            continue
        try:
            pname = (proc.info["name"] or "").lower()
            cmdline = proc.info["cmdline"] or []
            cmdline_lower = " ".join(cmdline).lower()
            if pname == my_name.lower() or (
                "python" in pname and script_name in cmdline_lower
            ):
                proc.kill()
                proc.wait(timeout=5)
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.TimeoutExpired):
            pass


def _setup_trayconsole(win):
    if not _trayconsole_available:
        return

    client = TrayConsoleClient("trayconsole_claude_monitor")

    @client.on("show")
    def handle_show():
        QTimer.singleShot(0, lambda: (win.show(), win.raise_(), win.activateWindow()))
        return {"ok": True}

    @client.on("hide")
    def handle_hide():
        QTimer.singleShot(0, win.hide_to_tray)
        return {"ok": True}

    @client.on("status")
    def handle_status():
        return {"status": "running", "visible": win.isVisible()}

    @client.on("shutdown")
    def handle_shutdown():
        QTimer.singleShot(0, QApplication.instance().quit)
        return {"status": "ok"}

    @client.on("custom:refresh")
    def handle_refresh():
        QTimer.singleShot(0, win._fetch)
        QTimer.singleShot(0, win._fetch_incidents)
        return {"ok": True}

    @client.on("custom:relogin")
    def handle_relogin():
        QTimer.singleShot(0, win._re_login)
        return {"ok": True}

    client.start()


def main():
    _kill_previous_instances()

    account_manager = AccountManager()
    account_manager.migrate_legacy()

    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)
    app.setStyleSheet("* { outline: none; }")

    tray = QSystemTrayIcon(_make_pct_icon(None), app)
    tray.setToolTip("Claude Usage")

    tray_menu = QMenu()
    tray_menu.setStyleSheet(
        "QMenu{background:#1a1a1a;border:1px solid #333;color:#ccc;font-size:11px;}"
        "QMenu::item:selected{background:#333;}"
    )

    win = UsageWindow(tray, account_manager)
    win._load_state()
    win.show()

    # Start CookieBridge server — always listening for extension pushes
    bridge = CookieBridgeServer(app)
    bridge.cookies_received.connect(win.on_bridge_cookies)
    bridge.start()
    app.aboutToQuit.connect(bridge.stop)

    _setup_trayconsole(win)

    act_show  = QAction("Показать / Скрыть", app)
    act_ref   = QAction("Обновить", app)
    act_quit  = QAction("Выйти", app)

    act_show.triggered.connect(lambda: win.show() if win.isHidden() else win.hide())
    act_ref.triggered.connect(win._fetch)
    act_quit.triggered.connect(app.quit)

    tray_menu.addAction(act_show)
    tray_menu.addAction(act_ref)
    tray_menu.addSeparator()
    tray_menu.addAction(act_quit)

    tray.setContextMenu(tray_menu)
    tray.activated.connect(
        lambda reason: (win.show() if win.isHidden() else win.hide())
        if reason == QSystemTrayIcon.Trigger else None
    )
    tray.show()

    # Fetch all accounts data in background on startup
    QTimer.singleShot(0, win._fetch_all_accounts)
    # Refresh identity for accounts that have no email/plan yet
    QTimer.singleShot(2000, win._refresh_missing_identities)
    # Associate current Claude Code CLI tokens with active account
    QTimer.singleShot(3000, win._associate_cc_tokens_on_startup)

    sys.exit(app.exec_())


if __name__ == "__main__":
    main()
