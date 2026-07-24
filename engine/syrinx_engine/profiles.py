"""Voice-profile storage — SQLite + reference-audio samples on disk.

A profile is a named voice, of one of two kinds:
  - **preset**: a built-in engine voice (e.g. Kokoro "af_heart").
  - **cloned**: one or more reference samples (audio + transcript) that a
    cloning engine (Qwen/LuxTTS/...) turns into a zero-shot clone.

Optional **personality** guides the LLM rewrite/compose (when that lands).
Grounded in the Voicebox profiles + profile_samples model, simplified.

Storage: $SYRINX_DATA_DIR/syrinx.db  (default per-OS data dir — see paths.py)
Samples: $SYRINX_DATA_DIR/profiles/<profile_id>/<sample_id>.wav
"""

import json
import os
import shutil
import sqlite3
import tempfile
import time
import uuid
import zipfile
from dataclasses import asdict, dataclass, field
from pathlib import Path

from .paths import data_dir as _data_dir


@dataclass
class Sample:
    id: str
    audio_path: str
    reference_text: str


@dataclass
class Profile:
    id: str
    name: str
    voice_type: str  # "preset" | "cloned"
    language: str = "en"
    description: str = ""
    personality: str = ""
    default_engine: str = ""  # cloned: e.g. "qwen"
    preset_engine: str = ""  # preset: e.g. "kokoro"
    preset_voice_id: str = ""  # preset: e.g. "af_heart"
    created_at: float = 0.0
    # avatar: original photo + a crop rect in source pixels — the app renders
    # the shape from these, no server-side image processing. mode "circle"
    # crops a square (avatar_side == width == height); mode "panel" crops a
    # tall rect (width avatar_side, height avatar_sh) shown as the card's
    # right third.
    avatar_path: str = ""
    avatar_mode: str = "circle"
    avatar_sx: int = 0
    avatar_sy: int = 0
    avatar_side: int = 0
    avatar_sh: int = 0
    samples: list = field(default_factory=list)

    def summary(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "voice_type": self.voice_type,
            "language": self.language,
            "description": self.description,
            "has_personality": bool(self.personality),
            "default_engine": self.default_engine,
            "preset_engine": self.preset_engine,
            "preset_voice_id": self.preset_voice_id,
            "avatar_path": self.avatar_path,
            "avatar_mode": self.avatar_mode,
            "avatar_sx": self.avatar_sx,
            "avatar_sy": self.avatar_sy,
            "avatar_side": self.avatar_side,
            "avatar_sh": self.avatar_sh,
        }

    def full(self) -> dict:
        d = self.summary()
        d.update(
            {
                "description": self.description,
                "personality": self.personality,
                "created_at": self.created_at,
                "samples": [asdict(s) for s in self.samples],
            }
        )
        return d


class ProfileStore:
    def __init__(self) -> None:
        self._dir = _data_dir()
        self._dir.mkdir(parents=True, exist_ok=True)
        self._db = str(self._dir / "syrinx.db")
        self._profiles_dir = self._dir / "profiles"
        self._profiles_dir.mkdir(exist_ok=True)
        self._init_schema()

    def _conn(self) -> sqlite3.Connection:
        c = sqlite3.connect(self._db)
        c.row_factory = sqlite3.Row
        c.execute("PRAGMA foreign_keys=ON")
        return c

    # --- avatar path portability (mirrors HistoryStore._rel/_abs, phase 1.4) --
    #
    # Avatars are stored RELATIVE to the data dir (as_posix) so a data snapshot
    # moved between machines/OSes still resolves. Legacy rows hold an absolute
    # path from the machine that created them; on read we resolve it, and if it
    # no longer exists we re-root its ``profiles/<id>/<file>`` tail under the
    # current data dir — a one-time lazy migration for restored snapshots.

    def _rel(self, p: str | Path) -> str:
        # forward slashes so the stored path stays portable across OSes
        # (Linux: as_posix() == the old absolute-relative value once joined).
        return Path(p).relative_to(self._dir).as_posix()

    def _reroot(self, stored: str, profile_id: str) -> Path | None:
        """Re-root a foreign absolute path onto this data dir by matching the
        ``profiles/<id>/<file>`` (or trailing ``profiles/...``) suffix."""
        norm = stored.replace("\\", "/")
        marker = f"profiles/{profile_id}/"
        i = norm.find(marker)
        if i == -1:
            i = norm.rfind("profiles/")
        return (self._dir / norm[i:]) if i != -1 else None

    def _resolve_avatar(self, stored: str, profile_id: str) -> str:
        """Map a stored avatar path to an absolute path on THIS machine.

        Blank stays blank. A path that exists as-is (Linux-native today, or a
        Windows-absolute row) is returned verbatim; a relative row resolves
        under the data dir; a stale foreign-absolute row is re-rooted. When
        nothing is on disk we still return the most likely intended location
        so the row is stable — the app renders a blank avatar when the file
        can't be opened."""
        if not stored:
            return ""
        # `self._dir / stored`: a relative row lands under the data dir; an
        # absolute row (with drive/root) replaces and is returned unchanged —
        # so on Linux an existing absolute path is byte-identical to before.
        cand = self._dir / stored
        if cand.exists():
            return str(cand)
        rerooted = self._reroot(stored, profile_id)
        if rerooted is not None and rerooted.exists():
            return str(rerooted)
        return str(rerooted) if rerooted is not None else str(cand)

    def _init_schema(self) -> None:
        with self._conn() as c:
            c.executescript(
                """
                CREATE TABLE IF NOT EXISTS profiles(
                    id TEXT PRIMARY KEY,
                    name TEXT UNIQUE NOT NULL,
                    voice_type TEXT NOT NULL,
                    language TEXT DEFAULT 'en',
                    description TEXT DEFAULT '',
                    personality TEXT DEFAULT '',
                    default_engine TEXT DEFAULT '',
                    preset_engine TEXT DEFAULT '',
                    preset_voice_id TEXT DEFAULT '',
                    created_at REAL
                );
                CREATE TABLE IF NOT EXISTS samples(
                    id TEXT PRIMARY KEY,
                    profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                    audio_path TEXT NOT NULL,
                    reference_text TEXT DEFAULT ''
                );
                """
            )
            # additive migrations for databases created before avatars
            for ddl in (
                "ALTER TABLE profiles ADD COLUMN avatar_path TEXT DEFAULT ''",
                "ALTER TABLE profiles ADD COLUMN avatar_sx INTEGER DEFAULT 0",
                "ALTER TABLE profiles ADD COLUMN avatar_sy INTEGER DEFAULT 0",
                "ALTER TABLE profiles ADD COLUMN avatar_side INTEGER DEFAULT 0",
                "ALTER TABLE profiles ADD COLUMN avatar_mode TEXT DEFAULT 'circle'",
                "ALTER TABLE profiles ADD COLUMN avatar_sh INTEGER DEFAULT 0",
            ):
                try:
                    c.execute(ddl)
                except sqlite3.OperationalError:
                    pass  # column already exists

    # --- profiles -------------------------------------------------------

    def create(
        self,
        name: str,
        voice_type: str,
        *,
        language: str = "en",
        description: str = "",
        personality: str = "",
        default_engine: str = "",
        preset_engine: str = "",
        preset_voice_id: str = "",
    ) -> str:
        if voice_type not in ("preset", "cloned"):
            raise ValueError("voice_type must be 'preset' or 'cloned'")
        pid = uuid.uuid4().hex[:12]
        with self._conn() as c:
            c.execute(
                "INSERT INTO profiles(id,name,voice_type,language,description,personality,"
                "default_engine,preset_engine,preset_voice_id,created_at) "
                "VALUES(?,?,?,?,?,?,?,?,?,?)",
                (
                    pid,
                    name,
                    voice_type,
                    language,
                    description,
                    personality,
                    default_engine,
                    preset_engine,
                    preset_voice_id,
                    time.time(),
                ),
            )
        return pid

    def _row_to_profile(self, row: sqlite3.Row, samples: list) -> Profile:
        return Profile(
            id=row["id"],
            name=row["name"],
            voice_type=row["voice_type"],
            language=row["language"],
            description=row["description"],
            personality=row["personality"],
            default_engine=row["default_engine"],
            preset_engine=row["preset_engine"],
            preset_voice_id=row["preset_voice_id"],
            created_at=row["created_at"] or 0.0,
            avatar_path=self._resolve_avatar(row["avatar_path"] or "", row["id"]),
            avatar_mode=row["avatar_mode"] or "circle",
            avatar_sx=row["avatar_sx"] or 0,
            avatar_sy=row["avatar_sy"] or 0,
            avatar_side=row["avatar_side"] or 0,
            avatar_sh=row["avatar_sh"] or 0,
            samples=samples,
        )

    def get(self, profile_id: str) -> Profile | None:
        with self._conn() as c:
            row = c.execute("SELECT * FROM profiles WHERE id=?", (profile_id,)).fetchone()
            if not row:
                return None
            samples = [
                Sample(s["id"], s["audio_path"], s["reference_text"])
                for s in c.execute(
                    "SELECT * FROM samples WHERE profile_id=?", (profile_id,)
                ).fetchall()
            ]
            return self._row_to_profile(row, samples)

    def list(self) -> list[Profile]:
        with self._conn() as c:
            rows = c.execute("SELECT * FROM profiles ORDER BY created_at").fetchall()
            return [self._row_to_profile(r, []) for r in rows]

    def update(self, profile_id: str, **fields) -> None:
        allowed = {"name", "description", "language", "personality", "default_engine"}
        sets = {k: v for k, v in fields.items() if k in allowed and v is not None}
        if not sets:
            return
        cols = ", ".join(f"{k}=?" for k in sets)
        with self._conn() as c:
            c.execute(
                f"UPDATE profiles SET {cols} WHERE id=?", (*sets.values(), profile_id)
            )

    def delete(self, profile_id: str) -> None:
        with self._conn() as c:
            c.execute("DELETE FROM profiles WHERE id=?", (profile_id,))
        shutil.rmtree(self._profiles_dir / profile_id, ignore_errors=True)

    def set_avatar(
        self, profile_id: str, src: str, mode: str, sx: int, sy: int, sw: int, sh: int
    ) -> None:
        """Store an avatar photo + crop rect (source px). mode: circle|panel.
        Empty ``src`` keeps the current photo and only updates the crop."""
        p = self.get(profile_id)
        if not p:
            raise ValueError(f"unknown profile: {profile_id}")
        # p.avatar_path is already resolved to an absolute path on this machine.
        path = p.avatar_path
        if src and src != p.avatar_path:
            dest_dir = self._profiles_dir / profile_id
            dest_dir.mkdir(parents=True, exist_ok=True)
            if p.avatar_path:
                Path(p.avatar_path).unlink(missing_ok=True)
            ext = Path(src).suffix.lower() or ".png"
            dest = dest_dir / f"avatar{ext}"
            shutil.copyfile(src, dest)
            path = str(dest)
        if mode not in ("circle", "panel"):
            mode = "circle"
        # Store RELATIVE to the data dir (portable across OSes); this also
        # migrates a legacy absolute row to relative the next time it's touched.
        stored = path
        if path:
            try:
                stored = self._rel(path)
            except ValueError:
                stored = path  # outside the data dir (shouldn't happen) — keep as-is
        with self._conn() as c:
            c.execute(
                "UPDATE profiles SET avatar_path=?, avatar_mode=?, avatar_sx=?, avatar_sy=?, "
                "avatar_side=?, avatar_sh=? WHERE id=?",
                (stored, mode, sx, sy, sw, sh, profile_id),
            )

    # --- export / import ------------------------------------------------

    def export_package(self, profile_id: str, dest: str) -> None:
        """Write a portable .zip: profile.json + samples/<id>.wav (Voicebox-style)."""
        p = self.get(profile_id)
        if not p:
            raise ValueError(f"unknown profile: {profile_id}")
        with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
            z.writestr("profile.json", json.dumps(p.full(), indent=2))
            for s in p.samples:
                src = Path(s.audio_path)
                if src.exists():
                    z.write(src, f"samples/{s.id}.wav")
            if p.avatar_path and Path(p.avatar_path).exists():
                z.write(p.avatar_path, f"avatar{Path(p.avatar_path).suffix.lower()}")

    def import_package(self, src: str) -> str:
        """Create a new profile from an exported .zip; returns the new id."""
        with zipfile.ZipFile(src) as z:
            meta = json.loads(z.read("profile.json"))
            # profiles.name is UNIQUE — de-dup with a numeric suffix
            existing = {p.name for p in self.list()}
            base = meta.get("name") or "Imported voice"
            name, n = base, 2
            while name in existing:
                name = f"{base} ({n})"
                n += 1
            pid = self.create(
                name,
                meta.get("voice_type", "cloned"),
                language=meta.get("language", "en"),
                description=meta.get("description", ""),
                personality=meta.get("personality", ""),
                default_engine=meta.get("default_engine", ""),
                preset_engine=meta.get("preset_engine", ""),
                preset_voice_id=meta.get("preset_voice_id", ""),
            )
            for s in meta.get("samples", []):
                try:
                    data = z.read(f"samples/{s['id']}.wav")
                except KeyError:
                    continue
                with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tf:
                    tf.write(data)
                    tmp = tf.name
                try:
                    self.add_sample(pid, tmp, s.get("reference_text", ""))
                finally:
                    os.unlink(tmp)
            if meta.get("avatar_path"):
                ext = Path(meta["avatar_path"]).suffix.lower() or ".png"
                try:
                    data = z.read(f"avatar{ext}")
                except KeyError:
                    data = None
                if data:
                    with tempfile.NamedTemporaryFile(suffix=ext, delete=False) as tf:
                        tf.write(data)
                        tmp = tf.name
                    try:
                        side = int(meta.get("avatar_side", 0))
                        self.set_avatar(
                            pid, tmp,
                            meta.get("avatar_mode", "circle"),
                            int(meta.get("avatar_sx", 0)),
                            int(meta.get("avatar_sy", 0)),
                            side,
                            int(meta.get("avatar_sh", 0)) or side,
                        )
                    finally:
                        os.unlink(tmp)
        return pid

    # --- samples --------------------------------------------------------

    def add_sample(self, profile_id: str, src_audio: str, reference_text: str) -> Sample:
        if not self.get(profile_id):
            raise ValueError(f"unknown profile: {profile_id}")
        sid = uuid.uuid4().hex[:12]
        dest_dir = self._profiles_dir / profile_id
        dest_dir.mkdir(parents=True, exist_ok=True)
        dest = dest_dir / f"{sid}.wav"
        shutil.copyfile(src_audio, dest)
        with self._conn() as c:
            c.execute(
                "INSERT INTO samples(id,profile_id,audio_path,reference_text) VALUES(?,?,?,?)",
                (sid, profile_id, str(dest), reference_text),
            )
        return Sample(sid, str(dest), reference_text)

    def sample_counts(self) -> dict:
        """profile_id -> number of reference samples (one query for ListProfiles)."""
        with self._conn() as c:
            return dict(
                c.execute("SELECT profile_id, COUNT(*) FROM samples GROUP BY profile_id")
            )

    def sample_path(self, sample_id: str) -> str:
        with self._conn() as c:
            row = c.execute(
                "SELECT audio_path FROM samples WHERE id=?", (sample_id,)
            ).fetchone()
            return row[0] if row else ""

    def set_sample_text(self, sample_id: str, reference_text: str) -> None:
        with self._conn() as c:
            c.execute(
                "UPDATE samples SET reference_text=? WHERE id=?", (reference_text, sample_id)
            )

    def delete_sample(self, sample_id: str) -> None:
        with self._conn() as c:
            row = c.execute("SELECT audio_path FROM samples WHERE id=?", (sample_id,)).fetchone()
            c.execute("DELETE FROM samples WHERE id=?", (sample_id,))
        if row:
            Path(row["audio_path"]).unlink(missing_ok=True)
