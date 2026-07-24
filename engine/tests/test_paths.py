"""Per-OS path resolution — the Linux byte-identity guarantee is the whole
point of this seam, so most of these tests pin the resolved strings to the
exact literals the engine used before ``paths.py`` existed.

The autouse ``isolated_env`` fixture sets ``SYRINX_DATA_DIR`` /
``XDG_CACHE_HOME`` / ``XDG_CONFIG_HOME``; each test clears or overrides what it
needs. Windows/macOS behavior is exercised by monkeypatching ``sys.platform``
and stubbing ``platformdirs`` so the assertions hold on any host OS.
"""

import sys
from pathlib import Path

import platformdirs

from syrinx_engine import paths


# --- Linux byte-identity (THE acceptance criterion) ----------------------


def test_linux_data_dir_is_the_historical_literal(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.delenv("SYRINX_DATA_DIR", raising=False)
    monkeypatch.delenv("XDG_DATA_HOME", raising=False)
    assert paths.data_dir() == Path.home() / ".local" / "share" / "syrinx"


def test_linux_data_dir_ignores_xdg_data_home(monkeypatch):
    # The old code hard-coded ~/.local/share/syrinx and never consulted
    # XDG_DATA_HOME. platformdirs *would* honor it — byte-identity means we
    # must keep ignoring it even when it is set.
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.delenv("SYRINX_DATA_DIR", raising=False)
    monkeypatch.setenv("XDG_DATA_HOME", "/somewhere/else/xdg-data")
    assert paths.data_dir() == Path.home() / ".local" / "share" / "syrinx"


def test_linux_worker_log_is_the_bare_cache_literal(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.delenv("XDG_CACHE_HOME", raising=False)
    for name in ("luxtts", "seedvc", "vevo"):
        assert paths.worker_log_path(name) == Path.home() / ".cache" / f"syrinx-{name}.log"


def test_linux_worker_log_ignores_xdg_cache_home(monkeypatch):
    # Same story as the data dir: the historical path was a bare ~/.cache file.
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setenv("XDG_CACHE_HOME", "/somewhere/else/xdg-cache")
    assert paths.worker_log_path("luxtts") == Path.home() / ".cache" / "syrinx-luxtts.log"


# --- SYRINX_DATA_DIR override wins everywhere -----------------------------


def test_override_wins_on_linux(monkeypatch, tmp_path):
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setenv("SYRINX_DATA_DIR", str(tmp_path / "custom"))
    assert paths.data_dir() == tmp_path / "custom"


def test_override_wins_on_windows(monkeypatch, tmp_path):
    monkeypatch.setattr(sys, "platform", "win32")
    monkeypatch.setenv("SYRINX_DATA_DIR", str(tmp_path / "custom"))
    assert paths.data_dir() == tmp_path / "custom"


def test_override_wins_on_macos(monkeypatch, tmp_path):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setenv("SYRINX_DATA_DIR", str(tmp_path / "custom"))
    assert paths.data_dir() == tmp_path / "custom"


# --- Windows / macOS: data lives under the platformdirs location ----------
# (stub platformdirs so the assertions are host-OS independent, and pin the
#  ("syrinx","syrinx") appname/appauthor contract from RPC-PROTOCOL §2.2)


def test_windows_data_dir_delegates_to_platformdirs(monkeypatch):
    monkeypatch.setattr(sys, "platform", "win32")
    monkeypatch.delenv("SYRINX_DATA_DIR", raising=False)
    seen = []

    def fake(appname, appauthor):
        seen.append((appname, appauthor))
        return r"C:\Users\u\AppData\Local\syrinx\syrinx"

    monkeypatch.setattr(platformdirs, "user_data_dir", fake)
    assert paths.data_dir() == Path(r"C:\Users\u\AppData\Local\syrinx\syrinx")
    assert seen == [("syrinx", "syrinx")]


def test_macos_data_dir_delegates_to_platformdirs(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.delenv("SYRINX_DATA_DIR", raising=False)
    monkeypatch.setattr(
        platformdirs, "user_data_dir",
        lambda *a, **k: "/Users/u/Library/Application Support/syrinx",
    )
    assert paths.data_dir() == Path("/Users/u/Library/Application Support/syrinx")


def test_windows_worker_log_under_platformdirs_cache(monkeypatch):
    monkeypatch.setattr(sys, "platform", "win32")
    seen = []

    def fake(appname, appauthor):
        seen.append((appname, appauthor))
        return r"C:\Users\u\AppData\Local\syrinx\syrinx\Cache"

    monkeypatch.setattr(platformdirs, "user_cache_dir", fake)
    got = paths.worker_log_path("seedvc")
    assert got == Path(r"C:\Users\u\AppData\Local\syrinx\syrinx\Cache") / "syrinx-seedvc.log"
    assert seen == [("syrinx", "syrinx")]


# --- the real current-platform path resolves and delegates correctly ------


def test_current_platform_matches_platformdirs_when_non_linux(monkeypatch):
    """On Win/mac hosts, data_dir() equals platformdirs' real output (this is
    the actual path on this box); on Linux this test is a no-op."""
    if sys.platform == "linux":
        return
    monkeypatch.delenv("SYRINX_DATA_DIR", raising=False)
    assert paths.data_dir() == Path(platformdirs.user_data_dir("syrinx", "syrinx"))


# --- the discovery file shares the engine's data root on Win/mac ----------


def test_discovery_shares_data_root_on_non_linux(monkeypatch):
    from syrinx_engine import rpc

    monkeypatch.setattr(sys, "platform", "win32")
    monkeypatch.delenv("SYRINX_RPC_ENDPOINT", raising=False)
    monkeypatch.delenv("SYRINX_DATA_DIR", raising=False)
    p = rpc.discovery_path()
    assert p.name == "rpc.json"
    assert p.parent == paths._default_data_dir()
