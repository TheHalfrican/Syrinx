; Syrinx — Windows installer (MULTIPLATPLAN §2.2).
;
; Per-user, no elevation: installs the dist bundle to
;   %LOCALAPPDATA%\Programs\Syrinx
; a Start-Menu "Syrinx" shortcut (via the windowless VBS launcher) + a
; "Syrinx first-run setup" shortcut, and an uninstaller that removes the install
; dir and shortcuts but PRESERVES %LOCALAPPDATA%\syrinx\syrinx (the user's data:
; voices, history, settings) — mirroring install.sh --uninstall's "leave the
; user's data untouched" philosophy.
;
; Build:  makensis packaging\windows\syrinx.nsi   ->  dist\SyrinxSetup-x64.exe
; The bundle dir can be overridden:  makensis /DBUNDLE=<path> ...

Unicode true
!include "MUI2.nsh"

!ifndef BUNDLE
  !define BUNDLE "..\..\dist\syrinx-windows-x64"
!endif
!ifndef OUTFILE
  !define OUTFILE "..\..\dist\SyrinxSetup-x64.exe"
!endif

!define APPNAME     "Syrinx"
!define COMPANY     "Syrinx"
!define ARPKEY      "Software\Microsoft\Windows\CurrentVersion\Uninstall\Syrinx"

Name "${APPNAME}"
OutFile "${OUTFILE}"
; Per-user default; /D=<dir> on the command line overrides (used by the
; scratch-install verification).
InstallDir "$LOCALAPPDATA\Programs\Syrinx"
RequestExecutionLevel user        ; no UAC — per-user install
SetCompressor /SOLID lzma

!define MUI_ICON   "syrinx.ico"
!define MUI_UNICON "syrinx.ico"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES

; Offer to run first-run right after install (pulls torch + the ML stack). A
; custom run-function is used because the command needs arguments — MUI's plain
; RUN+RUN_PARAMETERS path feeds them to Exec unquoted ("expects 1 parameter").
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Run Syrinx first-run setup now (downloads torch + the ML stack)"
!define MUI_FINISHPAGE_RUN_FUNCTION RunFirstRun
!insertmacro MUI_PAGE_FINISH

Function RunFirstRun
  Exec '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -ExecutionPolicy Bypass -File "$INSTDIR\syrinx-firstrun.ps1"'
FunctionEnd

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; --------------------------------------------------------------------------
Section "Install"
  SetOutPath "$INSTDIR"
  ; The whole torch-free bundle (app exe, embedded python + engine, sox, icon,
  ; launcher, first-run, wheel).
  File /r "${BUNDLE}\*"

  ; Start Menu: the app (windowless VBS wrapper sets sox PATH + SYRINX_ENGINE_CMD
  ; then launches syrinx-app.exe) and the first-run bootstrap.
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" \
    "$SYSDIR\wscript.exe" '"$INSTDIR\syrinx-launch.vbs"' \
    "$INSTDIR\syrinx.ico" 0
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME} first-run setup.lnk" \
    "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" \
    '-ExecutionPolicy Bypass -File "$INSTDIR\syrinx-firstrun.ps1"' \
    "$INSTDIR\syrinx.ico" 0

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Add/Remove Programs (per-user hive — no elevation needed).
  WriteRegStr   HKCU "${ARPKEY}" "DisplayName"     "${APPNAME}"
  WriteRegStr   HKCU "${ARPKEY}" "DisplayIcon"     "$INSTDIR\syrinx.ico"
  WriteRegStr   HKCU "${ARPKEY}" "Publisher"       "${COMPANY}"
  WriteRegStr   HKCU "${ARPKEY}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegStr   HKCU "${ARPKEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "${ARPKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${ARPKEY}" "NoRepair" 1
SectionEnd

; --------------------------------------------------------------------------
Section "Uninstall"
  ; Remove shortcuts + install tree. We deliberately DO NOT touch
  ; $LOCALAPPDATA\syrinx\syrinx (voices/history/settings/rpc.json) — the user's
  ; data survives an uninstall, same as install.sh leaving the checkout alone.
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME} first-run setup.lnk"
  RMDir  "$SMPROGRAMS\${APPNAME}"

  RMDir /r "$INSTDIR"

  DeleteRegKey HKCU "${ARPKEY}"
SectionEnd
