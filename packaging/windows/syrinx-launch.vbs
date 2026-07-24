' Syrinx launch wrapper (the Start-Menu shortcut points wscript.exe here).
'
' Two jobs the shortcut itself cannot do, both needed before the app spawns the
' engine (RPC-PROTOCOL §13.2 — the app is the engine's lifecycle manager):
'
'   1. Put the bundled sox on PATH. qwen-tts shells out to `sox` at import
'      (pysox _get_valid_formats); the engine inherits this process's PATH, so
'      prepending tools\ here is what lets the spawned engine find it.
'   2. Point SYRINX_ENGINE_CMD at the bundled engine console script, so the app
'      spawns THIS bundle's engine verbatim (§13.2 step 1) regardless of cwd.
'      (The engine/.venv layout also satisfies the app's exe-ancestor probe,
'      step 3, so this is belt-and-suspenders.)
'
' Run via wscript.exe => no console window flashes for the GUI app.

Option Explicit
Dim fso, sh, here, tools, engineExe, env
Set fso = CreateObject("Scripting.FileSystemObject")
Set sh  = CreateObject("WScript.Shell")

here      = fso.GetParentFolderName(WScript.ScriptFullName)
tools     = fso.BuildPath(here, "tools")
engineExe = fso.BuildPath(here, "engine\.venv\Scripts\syrinx-engine.exe")

' Modify THIS process's environment; children (the app, then the engine) inherit.
Set env = sh.Environment("PROCESS")
env("PATH") = tools & ";" & env("PATH")
env("SYRINX_ENGINE_CMD") = engineExe

' Launch the app windowless (0), do not wait (False) — the wrapper exits, the
' app keeps running and owns the engine child.
sh.CurrentDirectory = here
sh.Run """" & fso.BuildPath(here, "syrinx-app.exe") & """", 0, False
