"""ProfileStore — voices, reference samples, avatars and portable packages."""

import json
import sqlite3
import zipfile
from pathlib import Path

import pytest

from syrinx_engine import paths
from syrinx_engine.profiles import Profile, ProfileStore, _data_dir


def test_data_dir_follows_the_env(isolated_env, monkeypatch):
    assert _data_dir() == isolated_env
    monkeypatch.delenv("SYRINX_DATA_DIR")
    # env deleted → the per-OS default (byte-identical to ~/.local/share/syrinx
    # on Linux; see test_paths.py for the platform-by-platform identity proof).
    assert _data_dir() == paths._default_data_dir()


# --- profiles ------------------------------------------------------------


def test_create_get_list_update_delete():
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned", language="fr", description="namekian",
                       personality="terse", default_engine="qwen")
    p = store.get(pid)
    assert (p.name, p.voice_type, p.language) == ("Piccolo", "cloned", "fr")
    assert p.default_engine == "qwen"
    assert p.summary()["has_personality"] is True
    assert [x.id for x in store.list()] == [pid]

    store.update(pid, name="Nail", description="changed", not_a_column="ignored")
    assert store.get(pid).name == "Nail"
    assert store.get(pid).description == "changed"

    store.delete(pid)
    assert store.get(pid) is None
    assert store.list() == []


def test_update_with_nothing_updatable_is_a_no_op():
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    store.update(pid, not_a_column="x", name=None)
    assert store.get(pid).name == "Piccolo"


def test_create_rejects_an_unknown_voice_type():
    with pytest.raises(ValueError, match="voice_type"):
        ProfileStore().create("Bad", "hologram")


def test_preset_profiles_keep_their_engine_and_voice_id():
    store = ProfileStore()
    pid = store.create("Heart", "preset", preset_engine="kokoro", preset_voice_id="af_heart")
    s = store.get(pid).summary()
    assert (s["preset_engine"], s["preset_voice_id"]) == ("kokoro", "af_heart")


def test_reopening_the_store_reruns_the_additive_migrations(isolated_env):
    """Every construction re-issues the avatar ALTERs; the second one has to
    swallow "duplicate column" rather than blow up on boot."""
    ProfileStore()
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    assert store.get(pid).avatar_mode == "circle"


def test_a_pre_avatar_database_is_migrated(isolated_env):
    with sqlite3.connect(isolated_env / "syrinx.db") as c:
        c.executescript(
            """
            CREATE TABLE profiles(
                id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, voice_type TEXT NOT NULL,
                language TEXT DEFAULT 'en', description TEXT DEFAULT '',
                personality TEXT DEFAULT '', default_engine TEXT DEFAULT '',
                preset_engine TEXT DEFAULT '', preset_voice_id TEXT DEFAULT '',
                created_at REAL);
            """
        )
        c.execute("INSERT INTO profiles VALUES('old','Ancient','cloned','en','','','','','',1.0)")
    p = ProfileStore().get("old")
    assert p.name == "Ancient"
    assert (p.avatar_path, p.avatar_mode, p.avatar_side) == ("", "circle", 0)


# --- samples -------------------------------------------------------------


def test_samples_are_copied_in_and_counted(make_wav):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    s = store.add_sample(pid, str(make_wav("ref.wav")), "reference words")
    assert Path(s.audio_path).exists()
    assert store.sample_counts() == {pid: 1}
    assert store.sample_path(s.id) == s.audio_path
    assert store.get(pid).samples[0].reference_text == "reference words"

    store.set_sample_text(s.id, "corrected")
    assert store.get(pid).samples[0].reference_text == "corrected"

    store.delete_sample(s.id)
    assert store.get(pid).samples == []
    assert not Path(s.audio_path).exists()
    assert store.sample_path("nope") == ""
    store.delete_sample("nope")  # unknown id is a no-op


def test_add_sample_to_an_unknown_profile_raises(make_wav):
    with pytest.raises(ValueError, match="unknown profile"):
        ProfileStore().add_sample("nope", str(make_wav("ref.wav")), "")


def test_deleting_a_profile_takes_its_sample_directory(make_wav):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    s = store.add_sample(pid, str(make_wav("ref.wav")), "")
    store.delete(pid)
    assert not Path(s.audio_path).parent.exists()


# --- avatars -------------------------------------------------------------


def photo(tmp_path, name="face.png"):
    p = tmp_path / name
    p.write_bytes(b"\x89PNG\r\n\x1a\n fake image bytes")
    return p


def test_set_avatar_stores_the_photo_and_the_crop(tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    store.set_avatar(pid, str(photo(tmp_path)), "panel", 10, 20, 30, 40)
    p = store.get(pid)
    assert Path(p.avatar_path).exists()
    assert p.avatar_mode == "panel"
    assert (p.avatar_sx, p.avatar_sy, p.avatar_side, p.avatar_sh) == (10, 20, 30, 40)


def test_an_empty_src_only_re_crops(tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    store.set_avatar(pid, str(photo(tmp_path)), "circle", 0, 0, 100, 100)
    kept = store.get(pid).avatar_path
    store.set_avatar(pid, "", "circle", 5, 5, 50, 50)
    assert store.get(pid).avatar_path == kept
    assert store.get(pid).avatar_side == 50


def test_replacing_the_photo_removes_the_old_one(tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    store.set_avatar(pid, str(photo(tmp_path, "one.png")), "circle", 0, 0, 10, 10)
    first = Path(store.get(pid).avatar_path)
    store.set_avatar(pid, str(photo(tmp_path, "two.jpg")), "circle", 0, 0, 10, 10)
    assert store.get(pid).avatar_path.endswith(".jpg")
    assert not first.exists()


def test_an_unknown_avatar_mode_falls_back_to_circle(tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    store.set_avatar(pid, str(photo(tmp_path)), "hexagon", 0, 0, 10, 10)
    assert store.get(pid).avatar_mode == "circle"


def test_set_avatar_on_an_unknown_profile_raises(tmp_path):
    with pytest.raises(ValueError, match="unknown profile"):
        ProfileStore().set_avatar("nope", str(photo(tmp_path)), "circle", 0, 0, 1, 1)


# --- export / import -----------------------------------------------------


def test_export_import_round_trip(make_wav, tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned", personality="terse", language="fr")
    store.add_sample(pid, str(make_wav("ref.wav")), "reference words")
    store.set_avatar(pid, str(photo(tmp_path)), "panel", 1, 2, 3, 4)

    dest = tmp_path / "voice.zip"
    store.export_package(pid, str(dest))
    with zipfile.ZipFile(dest) as z:
        names = z.namelist()
        assert "profile.json" in names and "avatar.png" in names
        assert any(n.startswith("samples/") for n in names)
        assert json.loads(z.read("profile.json"))["personality"] == "terse"

    new_id = store.import_package(str(dest))
    imported = store.get(new_id)
    assert imported.name == "Piccolo (2)"  # names are UNIQUE — de-duped
    assert imported.personality == "terse"
    assert imported.language == "fr"
    assert len(imported.samples) == 1
    assert imported.avatar_mode == "panel"
    assert (imported.avatar_side, imported.avatar_sh) == (3, 4)


def test_import_dedups_repeatedly(make_wav, tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    dest = tmp_path / "voice.zip"
    store.export_package(pid, str(dest))
    store.import_package(str(dest))
    store.import_package(str(dest))
    assert {p.name for p in store.list()} == {"Piccolo", "Piccolo (2)", "Piccolo (3)"}


def test_import_tolerates_a_package_missing_its_payload(tmp_path):
    """Hand-rolled / truncated zips: skip what isn't there, still import."""
    dest = tmp_path / "voice.zip"
    with zipfile.ZipFile(dest, "w") as z:
        z.writestr("profile.json", json.dumps({
            "name": "Sparse", "voice_type": "cloned",
            "samples": [{"id": "missing", "reference_text": "x"}],
            "avatar_path": "/gone/face.png", "avatar_side": 10,
        }))
    store = ProfileStore()
    pid = store.import_package(str(dest))
    p = store.get(pid)
    assert p.name == "Sparse"
    assert p.samples == []
    assert p.avatar_path == ""


def test_export_of_an_unknown_profile_raises(tmp_path):
    with pytest.raises(ValueError, match="unknown profile"):
        ProfileStore().export_package("nope", str(tmp_path / "x.zip"))


def test_export_skips_samples_whose_audio_vanished(make_wav, tmp_path):
    store = ProfileStore()
    pid = store.create("Piccolo", "cloned")
    s = store.add_sample(pid, str(make_wav("ref.wav")), "")
    Path(s.audio_path).unlink()
    dest = tmp_path / "voice.zip"
    store.export_package(pid, str(dest))
    with zipfile.ZipFile(dest) as z:
        assert z.namelist() == ["profile.json"]


def test_full_carries_the_samples_and_summary_does_not():
    p = Profile(id="x", name="N", voice_type="cloned")
    assert "samples" not in p.summary()
    assert p.full()["samples"] == []
