#!/usr/bin/env python3
"""Claude Usage Monitor — compact tray overlay with direct API fetch."""

import sys
import os
import re
import json
import subprocess
from pathlib import Path
from datetime import datetime, timedelta


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
from PyQt5.QtCore import Qt, QTimer, QThread, pyqtSignal, QPoint
from PyQt5.QtGui import QIcon, QPixmap, QPainter, QColor, QFont, QCursor

# When frozen by PyInstaller, resolve paths relative to the EXE location
if getattr(sys, "frozen", False):
    _APP_DIR = Path(sys.executable).parent
else:
    _APP_DIR = Path(__file__).parent

AUTH_FILE = _APP_DIR / "claude_auth.json"
_STATE_FILE = _APP_DIR / "window_state.json"

_VENV_PY = _APP_DIR / ".venv" / "Scripts" / "python.exe"
_SUBPROCESS_PY = str(_VENV_PY) if _VENV_PY.exists() else sys.executable


# ---------------------------------------------------------------------------
# Login script (opens real Chrome for Google OAuth)
# ---------------------------------------------------------------------------

_LOGIN_SCRIPT = (
    'import sys\n'
    'from playwright.sync_api import sync_playwright\n'
    'auth_file = sys.argv[1]\n'
    'with sync_playwright() as pw:\n'
    '    browser = pw.chromium.launch(\n'
    '        channel="chrome",\n'
    '        headless=False,\n'
    '        args=[\n'
    '            "--disable-blink-features=AutomationControlled",\n'
    '            "--no-first-run",\n'
    '            "--no-default-browser-check",\n'
    '        ]\n'
    '    )\n'
    '    ctx = browser.new_context(no_viewport=True)\n'
    '    page = ctx.new_page()\n'
    '    page.add_init_script(\n'
    '        "Object.defineProperty(navigator,\'webdriver\',{get:()=>undefined})"\n'
    '    )\n'
    '    page.goto("https://claude.ai/login")\n'
    '    print("Waiting for login...", flush=True)\n'
    '    page.wait_for_url(\n'
    '        lambda u: "claude.ai" in u and "/login" not in u,\n'
    '        timeout=180000\n'
    '    )\n'
    '    page.goto("https://claude.ai/settings/usage", wait_until="networkidle")\n'
    '    ctx.storage_state(path=auth_file)\n'
    '    browser.close()\n'
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
    '        "https://claude.ai/api/append_message",\n'
    '        json={\n'
    '            "organization_uuid": org_uuid,\n'
    '            "conversation_uuid": conv_uuid,\n'
    '            "text": "hi",\n'
    '            "completion": {"prompt": "hi", "model": "claude-sonnet-4-20250514"},\n'
    '            "attachments": [],\n'
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
# Threads
# ---------------------------------------------------------------------------

class LoginThread(QThread):
    done   = pyqtSignal()
    failed = pyqtSignal(str)

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _LOGIN_SCRIPT, str(AUTH_FILE)],
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
    result          = pyqtSignal(list)
    error           = pyqtSignal(str)
    session_expired = pyqtSignal()

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _FETCH_SCRIPT, str(AUTH_FILE)],
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
                    self.session_expired.emit()
                else:
                    self.error.emit(err)
                return

            self.result.emit(self._parse(raw.get("usage", {})))
        except Exception as exc:
            import traceback
            self.error.emit(f"{exc}\n{traceback.format_exc()[-400:]}")

    # Map API keys → display names
    _CATEGORIES = {
        "five_hour":          ("Сессия", "session"),
        "seven_day":          ("Все модели", "weekly"),
        "seven_day_opus":     ("Opus", "opus"),
        "seven_day_sonnet":   ("Sonnet", "sonnet"),
        "seven_day_cowork":   ("Cowork", "cowork"),
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


class PingThread(QThread):
    done  = pyqtSignal()
    error = pyqtSignal(str)

    def run(self):
        try:
            proc = subprocess.run(
                [_SUBPROCESS_PY, "-c", _PING_SCRIPT, str(AUTH_FILE)],
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
    # Black square background with slight rounding
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
        # Larger font for 1-2 digit numbers, slightly smaller for 3 digits
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
# Overlay window
# ---------------------------------------------------------------------------

class UsageWindow(QWidget):

    def __init__(self, tray: QSystemTrayIcon):
        super().__init__()
        self._tray = tray
        self._drag = QPoint()
        self._drag_started = False
        self._just_compacted = False
        self._rows: list = []
        self._models: list = []
        self._compact_mode = False
        self._logging_in = False

        self.setWindowFlags(
            Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool
        )
        self.setAttribute(Qt.WA_TranslucentBackground)
        self.setAttribute(Qt.WA_DeleteOnClose, False)

        self._build()
        self._init_timers()

        if AUTH_FILE.exists():
            self._fetch()
        else:
            self._show_login()

    # ---- build ---------------------------------------------------

    def _build(self):
        outer = QVBoxLayout(self)
        outer.setContentsMargins(5, 5, 5, 5)

        self._card = _Card(self)
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

        # ── Body (rows) ─────────────────────────────────
        self._body_w = QWidget()
        self._body = QVBoxLayout(self._body_w)
        self._body.setContentsMargins(0, 0, 0, 0)
        self._body.setSpacing(2)

        self._placeholder = QLabel("загрузка\u2026")
        self._placeholder.setStyleSheet("color:#aaa;font-size:11px;")
        self._placeholder.setAlignment(Qt.AlignCenter)
        self._body.addWidget(self._placeholder)

        vbox.addWidget(self._body_w)
        self.adjustSize()

    # ---- timers --------------------------------------------------

    def _init_timers(self):
        self._tick_t = QTimer(self)
        self._tick_t.timeout.connect(self._tick)
        self._tick_t.start(1_000)

        self._refresh_t = QTimer(self)
        self._refresh_t.timeout.connect(self._auto_refresh)
        self._refresh_t.start(3 * 60 * 1_000)  # every 3 min (API is fast)

    def _auto_refresh(self):
        if AUTH_FILE.exists() and not self._logging_in:
            self._fetch()

    # ---- compact mode --------------------------------------------

    def _enter_compact(self):
        self._compact_mode = True
        self._just_compacted = True
        self._header_w.hide()
        self._body_w.hide()
        self._update_compact_lbl()
        self._compact_w.show()
        self._save_state()
        self.adjustSize()

    def _exit_compact(self):
        self._compact_mode = False
        self._compact_w.hide()
        self._header_w.show()
        self._body_w.show()
        self.adjustSize()
        self._save_state()

    def _update_compact_lbl(self):
        m = next(
            (x for x in self._models if x["name"] == "Сессия"),
            self._models[0] if self._models else None,
        )
        if not m:
            self._compact_lbl.setText("—")
            return
        pct = m["pct"]
        t = _fmt_remaining(m.get("reset_dt"))
        color = _pct_color(pct)
        self._compact_lbl.setText(f"{pct:.0f}%  ({t})" if t else f"{pct:.0f}%")
        self._compact_lbl.setStyleSheet(
            f"color:{color};font-size:11px;font-weight:600;"
        )

    # ---- login ---------------------------------------------------

    def _show_login(self):
        self._clear()
        lbl = QLabel("Нажми для входа в claude.ai")
        lbl.setStyleSheet(_SS_LOGIN_LBL)
        lbl.setAlignment(Qt.AlignCenter)
        self._body.addWidget(lbl)

        btn = QPushButton("Войти через Chrome")
        btn.setCursor(QCursor(Qt.PointingHandCursor))
        btn.setStyleSheet(_SS_LOGIN_BTN)
        btn.clicked.connect(self._start_login)
        self._body.addWidget(btn)
        self._status_lbl.setText("")
        self.adjustSize()

    def _start_login(self):
        if self._logging_in:
            return
        self._logging_in = True
        self._clear()
        lbl = QLabel("Chrome открыт, войди в аккаунт\u2026")
        lbl.setStyleSheet(_SS_DIM)
        lbl.setWordWrap(True)
        lbl.setAlignment(Qt.AlignCenter)
        self._body.addWidget(lbl)
        self._status_lbl.setText("вход\u2026")
        self.adjustSize()

        self._login_thread = LoginThread(self)
        self._login_thread.done.connect(self._on_login_done)
        self._login_thread.failed.connect(self._on_login_failed)
        self._login_thread.start()

    def _on_login_done(self):
        self._logging_in = False
        self._fetch()

    def _on_login_failed(self, msg):
        self._logging_in = False
        self._clear()
        lbl = QLabel(f"Ошибка: {msg[:120]}")
        lbl.setStyleSheet("color:#f87171;font-size:9px;")
        lbl.setWordWrap(True)
        self._body.addWidget(lbl)
        btn = QPushButton("Попробовать снова")
        btn.clicked.connect(self._show_login)
        self._body.addWidget(btn)
        self.adjustSize()

    # ---- fetch ---------------------------------------------------

    def _fetch(self):
        if not AUTH_FILE.exists():
            self._show_login()
            return
        self._status_lbl.setText("")
        self._thread = FetchThread(self)
        self._thread.result.connect(self._on_result)
        self._thread.error.connect(self._on_error)
        self._thread.session_expired.connect(self._show_login)
        self._thread.start()

    def _on_result(self, models):
        self._rebuild(models)
        self._status_lbl.setText("")

    def _on_error(self, msg):
        self._status_lbl.setText("err")
        self._clear()
        lbl = QLabel(msg[:180])
        lbl.setStyleSheet("color:#f87171;font-size:9px;")
        lbl.setWordWrap(True)
        self._body.addWidget(lbl)
        self.adjustSize()

    # ---- rebuild -------------------------------------------------

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
            for m in models:
                countdown = _fmt_remaining(m.get("reset_dt"))
                row = _ModelRow(m["name"], m["pct"], m.get("reset_dt"), countdown)
                self._rows.append(row)
                self._body.addWidget(row)
            self._update_tray(models)
        if self._compact_mode:
            self._update_compact_lbl()
        self.adjustSize()

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

    # ---- countdown -----------------------------------------------

    def _tick(self):
        if not self._rows:
            return
        need_refresh = False
        for row in self._rows:
            if row.reset_dt is None:
                continue
            secs = int((row.reset_dt - datetime.now()).total_seconds())
            if secs <= 0:
                need_refresh = True
            else:
                row.update_timer(_fmt_remaining(row.reset_dt))
        if self._compact_mode:
            self._update_compact_lbl()
        if need_refresh:
            self._fetch()

    # ---- window state persistence --------------------------------

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

    # ---- tray ----------------------------------------------------

    def hide_to_tray(self):
        self.hide()

    # ---- drag ----------------------------------------------------

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
        menu.addAction("Свернуть в трей", self.hide_to_tray)
        menu.addSeparator()
        menu.addAction("Войти снова", self._re_login)
        menu.addSeparator()
        menu.addAction("Выйти", QApplication.instance().quit)
        menu.exec_(e.globalPos())

    def _re_login(self):
        AUTH_FILE.unlink(missing_ok=True)
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
            # Only emit click if mouse didn't move (not a drag)
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
    """Kill any already-running ClaudeMonitor processes (except self)."""
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

            # Match by EXE name or by python running our script
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
        return {"ok": True}

    @client.on("custom:relogin")
    def handle_relogin():
        QTimer.singleShot(0, win._re_login)
        return {"ok": True}

    client.start()


def main():
    _kill_previous_instances()

    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)

    tray = QSystemTrayIcon(_make_pct_icon(None), app)
    tray.setToolTip("Claude Usage")

    tray_menu = QMenu()
    tray_menu.setStyleSheet(
        "QMenu{background:#1a1a1a;border:1px solid #333;color:#ccc;font-size:11px;}"
        "QMenu::item:selected{background:#333;}"
    )

    win = UsageWindow(tray)
    win._load_state()
    win.show()
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

    sys.exit(app.exec_())


if __name__ == "__main__":
    main()
