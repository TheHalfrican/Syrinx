"""Generation history — persisted clips (WAV + metadata) in SQLite.

Every ``Speak`` result is saved so history survives restarts and can be
replayed, starred, exported, regenerated or deleted. Grounded in Voicebox's
``generations`` model, simplified: no versions/effects chain yet (regenerate
produces a fresh entry rather than a stacked "take").

Storage: $SYRINX_DATA_DIR/syrinx.db   (table ``history``)
Audio:   $SYRINX_DATA_DIR/history/<id>.wav   (PCM16 mono; paths stored relative)
"""

import json
import shutil
import sqlite3
import time
import uuid
import wave
import zipfile
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from .profiles import _data_dir


@dataclass
class HistoryItem:
    id: str
    voice_id: str
    voice_name: str
    text: str
    audio_path: str  # relative to the data dir (portable)
    engine: str
    language: str
    duration: float
    starred: bool
    created_at: float
    tags: list = None  # user-defined labels (the Library filters on these)
    # conversion recipe (JSON: source/engine/mode/semitones/label + source
    # mtime/size) — lets Regenerate re-run the conversion instead of
    # re-speaking the transcript; "" for plain TTS rows
    vc_json: str = ""

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "voice_id": self.voice_id,
            "voice_name": self.voice_name,
            "text": self.text,
            "engine": self.engine,
            "language": self.language,
            "duration": self.duration,
            "starred": self.starred,
            "created_at": self.created_at,
            "tags": self.tags or [],
            # display string computed here — the app shows it verbatim
            "date": time.strftime("%b %d · %H:%M", time.localtime(self.created_at)),
        }


class HistoryStore:
    def __init__(self) -> None:
        self._dir = _data_dir()
        self._dir.mkdir(parents=True, exist_ok=True)
        self._db = str(self._dir / "syrinx.db")
        self._audio_dir = self._dir / "history"
        self._audio_dir.mkdir(exist_ok=True)
        self._init_schema()

    def _conn(self) -> sqlite3.Connection:
        c = sqlite3.connect(self._db)
        c.row_factory = sqlite3.Row
        return c

    def _init_schema(self) -> None:
        with self._conn() as c:
            c.executescript(
                """
                CREATE TABLE IF NOT EXISTS history(
                    id TEXT PRIMARY KEY,
                    voice_id TEXT NOT NULL,
                    voice_name TEXT DEFAULT '',
                    text TEXT NOT NULL,
                    audio_path TEXT NOT NULL,
                    engine TEXT DEFAULT '',
                    language TEXT DEFAULT 'en',
                    duration REAL DEFAULT 0,
                    starred INTEGER DEFAULT 0,
                    created_at REAL,
                    tags TEXT DEFAULT ''
                );
                """
            )
            # migrate rows created before tags / vc_json existed
            cols = [r[1] for r in c.execute("PRAGMA table_info(history)")]
            if "tags" not in cols:
                c.execute("ALTER TABLE history ADD COLUMN tags TEXT DEFAULT ''")
            if "vc_json" not in cols:
                c.execute("ALTER TABLE history ADD COLUMN vc_json TEXT DEFAULT ''")

    def _rel(self, p: Path) -> str:
        # forward slashes so the stored path stays portable across OSes
        # (on Linux as_posix() == str(); on Windows it avoids backslashes)
        return p.relative_to(self._dir).as_posix()

    def _abs(self, rel: str) -> Path:
        return self._dir / rel

    # --- write ----------------------------------------------------------

    def save_clip(
        self,
        *,
        voice_id: str,
        voice_name: str,
        text: str,
        pcm: bytes,  # float32 mono, as produced by the TTS backends
        sample_rate: int,
        engine: str = "",
        language: str = "en",
        vc_json: str = "",
    ) -> HistoryItem:
        hid = uuid.uuid4().hex[:12]
        dest = self._audio_dir / f"{hid}.wav"
        duration = self._write_wav(dest, pcm, sample_rate)
        rel = self._rel(dest)
        created = time.time()
        with self._conn() as c:
            c.execute(
                "INSERT INTO history(id,voice_id,voice_name,text,audio_path,engine,"
                "language,duration,starred,created_at,vc_json) VALUES(?,?,?,?,?,?,?,?,0,?,?)",
                (hid, voice_id, voice_name, text, rel, engine, language, duration, created, vc_json),
            )
        return HistoryItem(
            hid, voice_id, voice_name, text, rel, engine, language, duration, False, created,
            vc_json=vc_json,
        )

    @staticmethod
    def _write_wav(dest: Path, pcm: bytes, sample_rate: int) -> float:
        samples = np.frombuffer(pcm, dtype=np.float32)
        int16 = (np.clip(samples, -1.0, 1.0) * 32767.0).astype("<i2")
        with wave.open(str(dest), "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(sample_rate)
            w.writeframes(int16.tobytes())
        return samples.size / float(sample_rate) if sample_rate else 0.0

    # --- read -----------------------------------------------------------

    def _row(self, r: sqlite3.Row) -> HistoryItem:
        try:
            tags = json.loads(r["tags"] or "[]")
        except (json.JSONDecodeError, TypeError):
            tags = []
        return HistoryItem(
            id=r["id"],
            voice_id=r["voice_id"],
            voice_name=r["voice_name"],
            text=r["text"],
            audio_path=r["audio_path"],
            engine=r["engine"],
            language=r["language"],
            duration=r["duration"] or 0.0,
            starred=bool(r["starred"]),
            created_at=r["created_at"] or 0.0,
            tags=tags if isinstance(tags, list) else [],
            vc_json=r["vc_json"] or "",
        )

    def list(self) -> list[HistoryItem]:
        with self._conn() as c:
            rows = c.execute("SELECT * FROM history ORDER BY created_at DESC").fetchall()
            return [self._row(r) for r in rows]

    def get(self, hid: str) -> HistoryItem | None:
        with self._conn() as c:
            r = c.execute("SELECT * FROM history WHERE id=?", (hid,)).fetchone()
            return self._row(r) if r else None

    def read_pcm(self, hid: str) -> tuple[bytes, int] | None:
        """Load a stored clip back as float32 PCM for playback."""
        item = self.get(hid)
        if not item:
            return None
        path = self._abs(item.audio_path)
        if not path.exists():
            return None
        with wave.open(str(path), "rb") as w:
            rate = w.getframerate()
            frames = w.readframes(w.getnframes())
        floats = np.frombuffer(frames, dtype="<i2").astype(np.float32) / 32768.0
        return floats.tobytes(), rate

    # --- mutate ---------------------------------------------------------

    def trim(self, hid: str, start_s: float, end_s: float) -> bool:
        """Cut a clip's audio to [start_s, end_s) in place; the row keeps its
        text/tags/star and the duration column follows the new length."""
        item = self.get(hid)
        if not item:
            return False
        path = self._abs(item.audio_path)
        if not path.exists():
            return False
        with wave.open(str(path), "rb") as w:
            rate = w.getframerate()
            frames = w.readframes(w.getnframes())
        a = max(0, int(start_s * rate)) * 2  # PCM16 mono: 2 bytes/frame
        b = min(len(frames), int(end_s * rate) * 2)
        if b - a < int(0.1 * rate) * 2:
            return False
        with wave.open(str(path), "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(rate)
            w.writeframes(frames[a:b])
        duration = (b - a) / 2 / rate
        with self._conn() as c:
            c.execute("UPDATE history SET duration=? WHERE id=?", (duration, hid))
        return True

    def set_starred(self, hid: str, starred: bool) -> None:
        with self._conn() as c:
            c.execute("UPDATE history SET starred=? WHERE id=?", (1 if starred else 0, hid))

    def set_tags(self, hid: str, tags: list) -> None:
        with self._conn() as c:
            c.execute("UPDATE history SET tags=? WHERE id=?", (json.dumps(tags), hid))

    def delete(self, hid: str) -> None:
        item = self.get(hid)
        with self._conn() as c:
            c.execute("DELETE FROM history WHERE id=?", (hid,))
        if item:
            self._abs(item.audio_path).unlink(missing_ok=True)

    # --- export ---------------------------------------------------------

    def export_package(self, hid: str, dest: str) -> None:
        """Write a .zip with manifest.json + audio/clip.wav (Voicebox-style)."""
        item = self.get(hid)
        if not item:
            raise ValueError(f"unknown history id: {hid}")
        audio = self._abs(item.audio_path)
        manifest = item.to_dict()
        with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
            z.writestr("manifest.json", json.dumps(manifest, indent=2))
            if audio.exists():
                z.write(audio, "audio/clip.wav")

    def audio_abs_path(self, hid: str) -> str:
        """Absolute path of a clip's WAV (for the app to copy on export-audio)."""
        item = self.get(hid)
        return str(self._abs(item.audio_path)) if item else ""


@dataclass
class CaptureItem:
    id: str
    text: str
    created_at: float
    updated_at: float

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "text": self.text,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            # display string computed here — the app shows it verbatim
            "date": time.strftime("%b %d · %H:%M", time.localtime(self.created_at)),
        }


class CaptureStore:
    """Transcription captures — text only (no audio), table ``captures``.

    Saved from the Transcription view; an update replaces the text of the
    same row rather than creating a new one.
    """

    def __init__(self) -> None:
        self._dir = _data_dir()
        self._dir.mkdir(parents=True, exist_ok=True)
        self._db = str(self._dir / "syrinx.db")
        self._init_schema()

    def _conn(self) -> sqlite3.Connection:
        c = sqlite3.connect(self._db)
        c.row_factory = sqlite3.Row
        return c

    def _init_schema(self) -> None:
        with self._conn() as c:
            c.executescript(
                """
                CREATE TABLE IF NOT EXISTS captures(
                    id TEXT PRIMARY KEY,
                    text TEXT NOT NULL,
                    created_at REAL,
                    updated_at REAL
                );
                """
            )

    def save(self, text: str) -> CaptureItem:
        cid = uuid.uuid4().hex[:12]
        now = time.time()
        with self._conn() as c:
            c.execute(
                "INSERT INTO captures(id,text,created_at,updated_at) VALUES(?,?,?,?)",
                (cid, text, now, now),
            )
        return CaptureItem(cid, text, now, now)

    def list(self) -> list[CaptureItem]:
        with self._conn() as c:
            rows = c.execute("SELECT * FROM captures ORDER BY created_at DESC").fetchall()
            return [
                CaptureItem(r["id"], r["text"], r["created_at"] or 0.0, r["updated_at"] or 0.0)
                for r in rows
            ]

    def update(self, cid: str, text: str) -> None:
        with self._conn() as c:
            c.execute(
                "UPDATE captures SET text=?, updated_at=? WHERE id=?", (text, time.time(), cid)
            )

    def delete(self, cid: str) -> None:
        with self._conn() as c:
            c.execute("DELETE FROM captures WHERE id=?", (cid,))


def _audio_duration(path: str) -> float:
    """Best-effort duration in seconds (0.0 for containers soundfile can't read)."""
    try:
        import soundfile as sf

        info = sf.info(path)
        return info.frames / float(info.samplerate or 1)
    except Exception:  # noqa: BLE001
        return 0.0


@dataclass
class SourceClipItem:
    id: str
    name: str
    duration: float
    created_at: float
    path: str  # absolute — the app arms it directly as a conversion source
    transcript: str  # cached whisper output — re-arming skips re-transcription
    kind: str = "speech"  # "speech" | "music" — the vc-mode active at save time

    def to_dict(self) -> dict:
        mins, secs = divmod(int(round(self.duration)), 60)
        return {
            "id": self.id,
            "name": self.name,
            "duration": self.duration,
            "created_at": self.created_at,
            "path": self.path,
            "transcript": self.transcript,
            "kind": self.kind,
            # display string computed here — the app shows it verbatim
            "meta": f"{mins}:{secs:02d} · "
            + time.strftime("%b %d · %H:%M", time.localtime(self.created_at)),
        }


class SourceClipStore:
    """Voice-changer source clips — named recordings/imports kept for reuse.

    Unlike history rows these are *inputs*, not results. Audio is copied in
    on save (recordings live in the volatile runtime dir otherwise):
    $SYRINX_DATA_DIR/clips/<id>.<ext>, table ``source_clips``.
    """

    def __init__(self) -> None:
        self._dir = _data_dir()
        self._clips = self._dir / "clips"
        self._clips.mkdir(parents=True, exist_ok=True)
        self._db = str(self._dir / "syrinx.db")
        self._init_schema()

    def _conn(self) -> sqlite3.Connection:
        c = sqlite3.connect(self._db)
        c.row_factory = sqlite3.Row
        return c

    def _init_schema(self) -> None:
        with self._conn() as c:
            c.executescript(
                """
                CREATE TABLE IF NOT EXISTS source_clips(
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    filename TEXT NOT NULL,
                    duration REAL,
                    created_at REAL,
                    transcript TEXT DEFAULT '',
                    kind TEXT NOT NULL DEFAULT 'speech'
                );
                """
            )
            # migrate rows created before the transcript / kind columns existed
            cols = [r[1] for r in c.execute("PRAGMA table_info(source_clips)")]
            if "transcript" not in cols:
                c.execute("ALTER TABLE source_clips ADD COLUMN transcript TEXT DEFAULT ''")
            if "kind" not in cols:
                c.execute(
                    "ALTER TABLE source_clips ADD COLUMN kind TEXT NOT NULL DEFAULT 'speech'"
                )

    def save(
        self, src_path: str, name: str, transcript: str = "", kind: str = "speech"
    ) -> SourceClipItem:
        cid = uuid.uuid4().hex[:12]
        now = time.time()
        ext = Path(src_path).suffix.lower() or ".wav"
        fname = f"{cid}{ext}"
        dest = self._clips / fname
        shutil.copy2(src_path, dest)
        duration = _audio_duration(str(dest))
        name = name.strip() or time.strftime("clip %H:%M:%S", time.localtime(now))
        kind = kind if kind in ("speech", "music") else "speech"
        with self._conn() as c:
            c.execute(
                "INSERT INTO source_clips(id,name,filename,duration,created_at,transcript,kind)"
                " VALUES(?,?,?,?,?,?,?)",
                (cid, name, fname, duration, now, transcript, kind),
            )
        return SourceClipItem(cid, name, duration, now, str(dest), transcript, kind)

    def set_transcript(self, clip_id: str, transcript: str) -> None:
        """Backfill the cache for a clip saved before transcription finished."""
        with self._conn() as c:
            c.execute(
                "UPDATE source_clips SET transcript=? WHERE id=?", (transcript, clip_id)
            )

    def update_duration_for_path(self, path: str, duration: float) -> bool:
        """Refresh the stored duration of a clip whose WAV was rewritten in
        place (trim). *path* only matches when it resolves inside the clips
        dir, keyed by filename; returns True when a row was updated. Mirrors
        HistoryStore.trim's duration-follows-the-cut idiom for source clips."""
        p = Path(path)
        try:
            if p.parent.resolve() != self._clips.resolve():
                return False
        except OSError:
            return False
        with self._conn() as c:
            cur = c.execute(
                "UPDATE source_clips SET duration=? WHERE filename=?", (duration, p.name)
            )
            return cur.rowcount > 0

    def list(self) -> list[SourceClipItem]:
        with self._conn() as c:
            rows = c.execute(
                "SELECT * FROM source_clips ORDER BY created_at DESC"
            ).fetchall()
            return [
                SourceClipItem(
                    r["id"], r["name"], r["duration"] or 0.0,
                    r["created_at"] or 0.0, str(self._clips / r["filename"]),
                    r["transcript"] or "", r["kind"] or "speech",
                )
                for r in rows
            ]

    def delete(self, clip_id: str) -> None:
        with self._conn() as c:
            row = c.execute(
                "SELECT filename FROM source_clips WHERE id=?", (clip_id,)
            ).fetchone()
            c.execute("DELETE FROM source_clips WHERE id=?", (clip_id,))
        if row:
            (self._clips / row["filename"]).unlink(missing_ok=True)
