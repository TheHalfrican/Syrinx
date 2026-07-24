"""HistoryStore / CaptureStore / SourceClipStore — SQLite + WAVs on disk."""

import json
import sqlite3
import wave
from pathlib import Path

import numpy as np
import pytest

from syrinx_engine.history import CaptureStore, HistoryStore, SourceClipStore

RATE = 24_000


def _tone(secs=1.0, rate=RATE, amp=0.5):
    t = np.linspace(0, secs, int(secs * rate), endpoint=False, dtype=np.float32)
    return (np.sin(2 * np.pi * 440 * t) * amp).astype(np.float32).tobytes()


def _save(store, **over):
    kw = dict(
        voice_id="v1", voice_name="Piccolo", text="hello",
        pcm=_tone(), sample_rate=RATE, engine="kokoro", language="en",
    )
    kw.update(over)
    return store.save_clip(**kw)


# --- save / read ---------------------------------------------------------


def test_save_clip_writes_a_wav_with_the_right_duration():
    store = HistoryStore()
    item = _save(store, pcm=_tone(secs=2.0))
    assert item.duration == pytest.approx(2.0)
    path = Path(store.audio_abs_path(item.id))
    assert path.exists() and path.suffix == ".wav"
    with wave.open(str(path), "rb") as w:
        assert w.getnchannels() == 1
        assert w.getsampwidth() == 2
        assert w.getframerate() == RATE
        assert w.getnframes() == 2 * RATE


def test_list_and_get_round_trip():
    store = HistoryStore()
    a = _save(store, text="first")
    b = _save(store, text="second")
    ids = {h.id for h in store.list()}
    assert ids == {a.id, b.id}
    got = store.get(a.id)
    assert got.text == "first"
    assert got.voice_name == "Piccolo"
    assert store.get("nope") is None


def test_read_pcm_round_trips_through_int16():
    store = HistoryStore()
    pcm = _tone(secs=0.25)
    item = _save(store, pcm=pcm)
    back, rate = store.read_pcm(item.id)
    assert rate == RATE
    orig = np.frombuffer(pcm, dtype=np.float32)
    got = np.frombuffer(back, dtype=np.float32)
    assert got.shape == orig.shape
    # PCM16 quantization: one LSB of a 16-bit scale, plus the /32767-vs-/32768
    # asymmetry between write and read
    assert np.max(np.abs(got - orig)) < 1e-3


def test_read_pcm_missing_row_or_missing_file_is_none():
    store = HistoryStore()
    assert store.read_pcm("nope") is None
    item = _save(store)
    Path(store.audio_abs_path(item.id)).unlink()
    assert store.read_pcm(item.id) is None


def test_audio_path_is_stored_relative_so_the_data_dir_stays_portable():
    store = HistoryStore()
    item = _save(store)
    assert not Path(item.audio_path).is_absolute()
    assert item.audio_path.startswith("history/")


# --- mutate --------------------------------------------------------------


def test_set_starred_toggles():
    store = HistoryStore()
    item = _save(store)
    assert store.get(item.id).starred is False
    store.set_starred(item.id, True)
    assert store.get(item.id).starred is True
    store.set_starred(item.id, False)
    assert store.get(item.id).starred is False


def test_set_tags_round_trips_as_json():
    store = HistoryStore()
    item = _save(store)
    assert store.get(item.id).tags == []
    store.set_tags(item.id, ["demo", "keep"])
    assert store.get(item.id).tags == ["demo", "keep"]
    assert store.get(item.id).to_dict()["tags"] == ["demo", "keep"]
    store.set_tags(item.id, [])
    assert store.get(item.id).tags == []


def test_corrupt_tags_column_degrades_to_an_empty_list():
    store = HistoryStore()
    item = _save(store)
    with sqlite3.connect(store._db) as c:
        c.execute("UPDATE history SET tags=? WHERE id=?", ("{not json", item.id))
    assert store.get(item.id).tags == []


def test_delete_removes_the_row_and_the_file():
    store = HistoryStore()
    item = _save(store)
    path = Path(store.audio_abs_path(item.id))
    store.delete(item.id)
    assert store.get(item.id) is None
    assert not path.exists()
    store.delete("nope")  # deleting nothing is not an error


def test_trim_cuts_in_place_and_updates_the_duration():
    store = HistoryStore()
    item = _save(store, pcm=_tone(secs=3.0))
    assert store.trim(item.id, 0.5, 2.0) is True
    assert store.get(item.id).duration == pytest.approx(1.5, abs=1e-3)
    with wave.open(store.audio_abs_path(item.id), "rb") as w:
        assert w.getnframes() == pytest.approx(1.5 * RATE, abs=2)


def test_trim_keeps_the_row_identity():
    store = HistoryStore()
    item = _save(store, text="keep me")
    store.set_tags(item.id, ["tag"])
    store.set_starred(item.id, True)
    store.trim(item.id, 0.0, 0.5)
    got = store.get(item.id)
    assert (got.text, got.tags, got.starred) == ("keep me", ["tag"], True)


def test_trim_refuses_selections_under_a_tenth_of_a_second():
    store = HistoryStore()
    item = _save(store)
    assert store.trim(item.id, 0.1, 0.15) is False
    assert store.get(item.id).duration == pytest.approx(1.0)


def test_trim_refuses_unknown_ids_and_missing_files():
    store = HistoryStore()
    assert store.trim("nope", 0.0, 1.0) is False
    item = _save(store)
    Path(store.audio_abs_path(item.id)).unlink()
    assert store.trim(item.id, 0.0, 1.0) is False


# --- vc_json (the conversion recipe Regenerate replays) ------------------


def test_vc_json_defaults_to_empty_and_round_trips():
    store = HistoryStore()
    assert store.get(_save(store).id).vc_json == ""
    recipe = json.dumps({"source": "/tmp/x.wav", "engine": "seed_vc", "mode": "speech"})
    item = _save(store, vc_json=recipe)
    assert json.loads(store.get(item.id).vc_json)["engine"] == "seed_vc"


# --- export / misc -------------------------------------------------------


def test_export_package_and_audio_path(tmp_path):
    import zipfile

    store = HistoryStore()
    item = _save(store)
    dest = tmp_path / "clip.zip"
    store.export_package(item.id, str(dest))
    with zipfile.ZipFile(dest) as z:
        assert set(z.namelist()) == {"manifest.json", "audio/clip.wav"}
        assert json.loads(z.read("manifest.json"))["id"] == item.id
    with pytest.raises(ValueError):
        store.export_package("nope", str(dest))
    assert store.audio_abs_path("nope") == ""


def test_to_dict_carries_a_prerendered_date():
    store = HistoryStore()
    d = _save(store).to_dict()
    assert d["tags"] == [] and d["date"]


# --- schema migration ----------------------------------------------------


def test_opening_a_pre_tags_pre_vc_json_db_migrates_it(isolated_env):
    """Rows written before tags/vc_json existed must survive the upgrade."""
    db = isolated_env / "syrinx.db"
    with sqlite3.connect(db) as c:
        c.executescript(
            """
            CREATE TABLE history(
                id TEXT PRIMARY KEY,
                voice_id TEXT NOT NULL,
                voice_name TEXT DEFAULT '',
                text TEXT NOT NULL,
                audio_path TEXT NOT NULL,
                engine TEXT DEFAULT '',
                language TEXT DEFAULT 'en',
                duration REAL DEFAULT 0,
                starred INTEGER DEFAULT 0,
                created_at REAL
            );
            """
        )
        c.execute(
            "INSERT INTO history VALUES('old1','v1','Nail','ancient',"
            "'history/old1.wav','kokoro','en',1.5,1,1000.0)"
        )

    store = HistoryStore()  # constructing it runs the migration

    with sqlite3.connect(db) as c:
        cols = {r[1] for r in c.execute("PRAGMA table_info(history)")}
    assert {"tags", "vc_json"} <= cols

    item = store.get("old1")
    assert item.text == "ancient"
    assert item.starred is True
    assert item.tags == []
    assert item.vc_json == ""


# --- captures ------------------------------------------------------------


def test_capture_store_crud():
    store = CaptureStore()
    a = store.save("dictated text")
    b = store.save("another")
    assert {c.id for c in store.list()} == {a.id, b.id}
    store.update(a.id, "edited")
    assert next(c.text for c in store.list() if c.id == a.id) == "edited"
    store.delete(b.id)
    assert [c.id for c in store.list()] == [a.id]
    assert store.list()[0].to_dict()["date"]


# --- source clips --------------------------------------------------------


def test_source_clip_save_copies_the_file_in(make_wav):
    store = SourceClipStore()
    src = make_wav("rec.wav", secs=0.75)
    item = store.save(str(src), "Take 1")
    copied = Path(item.path)
    assert copied.exists() and copied != src
    assert copied.parent.name == "clips"
    assert item.duration == pytest.approx(0.75, abs=1e-3)
    src.unlink()  # the store owns its own copy now
    assert Path(store.list()[0].path).exists()


def test_source_clip_blank_name_gets_a_time_based_default(make_wav):
    store = SourceClipStore()
    item = store.save(str(make_wav("rec.wav", secs=0.2)), "   ")
    assert item.name.startswith("clip ")


def test_source_clip_transcript_cache(make_wav):
    store = SourceClipStore()
    item = store.save(str(make_wav("rec.wav", secs=0.2)), "Take", transcript="hi")
    assert store.list()[0].transcript == "hi"
    store.set_transcript(item.id, "corrected")
    assert store.list()[0].transcript == "corrected"


def test_source_clip_delete_removes_the_file(make_wav):
    store = SourceClipStore()
    item = store.save(str(make_wav("rec.wav", secs=0.2)), "Take")
    path = Path(item.path)
    store.delete(item.id)
    assert store.list() == []
    assert not path.exists()
    store.delete("nope")  # unknown id is a no-op


def test_a_clip_soundfile_cannot_read_gets_a_zero_duration(tmp_path):
    """m4a/webm imports: the row still saves, the duration just reads 0."""
    blob = tmp_path / "voice.m4a"
    blob.write_bytes(b"not actually audio")
    item = SourceClipStore().save(str(blob), "Import")
    assert item.duration == 0.0
    assert Path(item.path).suffix == ".m4a"


def test_a_pre_transcript_source_clips_table_is_migrated(isolated_env):
    with sqlite3.connect(isolated_env / "syrinx.db") as c:
        c.executescript(
            """
            CREATE TABLE source_clips(
                id TEXT PRIMARY KEY, name TEXT NOT NULL, filename TEXT NOT NULL,
                duration REAL, created_at REAL);
            """
        )
        c.execute("INSERT INTO source_clips VALUES('old','Take','old.wav',2.0,1000.0)")
    rows = SourceClipStore().list()
    assert [r.id for r in rows] == ["old"]
    # both the transcript and the later kind column are lazily added; the
    # pre-existing row picks up their defaults
    assert rows[0].transcript == ""
    assert rows[0].kind == "speech"


def test_source_clip_kind_is_stored_and_defaults_to_speech(make_wav):
    store = SourceClipStore()
    a = store.save(str(make_wav("a.wav", secs=0.2)), "Speech clip")
    b = store.save(str(make_wav("b.wav", secs=0.2)), "Cover", kind="music")
    by_id = {r.id: r for r in store.list()}
    assert by_id[a.id].kind == "speech"  # default
    assert by_id[b.id].kind == "music"


def test_update_duration_for_path_refreshes_a_stored_clip(make_wav):
    store = SourceClipStore()
    item = store.save(str(make_wav("rec.wav", secs=2.0)), "Take")
    assert store.update_duration_for_path(item.path, 1.5) is True
    assert store.list()[0].duration == pytest.approx(1.5)


def test_update_duration_for_path_ignores_paths_outside_the_clip_store(make_wav, tmp_path):
    store = SourceClipStore()
    store.save(str(make_wav("rec.wav", secs=2.0)), "Take")
    # a scratch/sibling path (not in clips/) matches nothing → no-op, False
    assert store.update_duration_for_path(str(tmp_path / "elsewhere.wav"), 0.5) is False
    assert store.list()[0].duration == pytest.approx(2.0)


def test_source_clip_to_dict_has_a_rendered_meta_line(make_wav):
    store = SourceClipStore()
    store.save(str(make_wav("rec.wav", secs=0.2)), "Take")
    meta = store.list()[0].to_dict()["meta"]
    assert meta.startswith("0:00 · ")
