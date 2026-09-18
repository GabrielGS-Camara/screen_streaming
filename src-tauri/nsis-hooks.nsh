; Runs after the installer finishes copying files (NSIS_HOOK_POSTINSTALL,
; see tauri.conf.json's bundle.windows.nsis.installerHooks). Verifies the
; FFmpeg DLLs the app links against at load time actually made it to disk.
;
; Why this exists: a real user's installer produced a correct package (the
; DLLs were confirmed present in it), but the app failed to launch on the
; machine it was installed on with "avcodec-63.dll não foi encontrado" —
; most likely a false-positive antivirus quarantine of a codec-named DLL
; (a well-known pattern; some AV heuristics are trigger-happy about
; "avcodec"-style names since malware sometimes disguises itself that way),
; either during extraction or via a delayed cloud-reputation scan right
; after. Because these DLLs are linked implicitly (resolved by the OS
; loader before our own code ever runs), the app itself can't detect or
; explain a missing one — it just fails to start with a generic Windows
; error. Checking right after install, while the user is still looking at
; the installer and in a position to act (whitelist the folder, reinstall),
; is much more actionable than a cryptic crash the next time they try to
; open the app.
;
; Only native NSIS instructions are used here (no !include, no LogicLib
; macros) so this doesn't depend on what Tauri's own base template already
; imports.

!macro NSIS_HOOK_POSTINSTALL
  Push $0
  StrCpy $0 ""

  IfFileExists "$INSTDIR\avcodec-63.dll" +2 0
    StrCpy $0 "$0avcodec-63.dll$\n"
  IfFileExists "$INSTDIR\avdevice-63.dll" +2 0
    StrCpy $0 "$0avdevice-63.dll$\n"
  IfFileExists "$INSTDIR\avfilter-12.dll" +2 0
    StrCpy $0 "$0avfilter-12.dll$\n"
  IfFileExists "$INSTDIR\avformat-63.dll" +2 0
    StrCpy $0 "$0avformat-63.dll$\n"
  IfFileExists "$INSTDIR\avutil-61.dll" +2 0
    StrCpy $0 "$0avutil-61.dll$\n"
  IfFileExists "$INSTDIR\swresample-7.dll" +2 0
    StrCpy $0 "$0swresample-7.dll$\n"
  IfFileExists "$INSTDIR\swscale-10.dll" +2 0
    StrCpy $0 "$0swscale-10.dll$\n"

  StrCmp $0 "" nsis_hook_postinstall_done 0
    MessageBox MB_ICONEXCLAMATION|MB_OK "Atenção: alguns arquivos necessários não foram encontrados após a instalação:$\n$\n$0$\nIsso costuma acontecer quando o antivírus remove esses arquivos por engano (falso positivo comum com DLLs de codec de vídeo). O programa NÃO vai abrir sem eles.$\n$\nAdicione uma exceção para esta pasta no seu antivírus e instale novamente:$\n$INSTDIR"

  nsis_hook_postinstall_done:
  Pop $0

  ; Visual C++ Redistributable (2015-2022 x64, still versioned "14.0"
  ; since Microsoft unified them at VS2015) — the Rust binary dynamically
  ; links against its runtime DLLs. Most real machines already have it
  ; (huge number of apps depend on it), so this only actually runs the
  ; (~10-20s) installer when the registry key Microsoft itself documents
  ; for detecting it is missing, instead of unconditionally reinstalling
  ; it on every "screen_streaming" install.
  Push $1
  Push $2
  ; The X64 runtime's key only ever lives in the 64-bit registry view —
  ; force that view explicitly rather than relying on this installer
  ; process's own bitness (a 32-bit installer would otherwise silently
  ; read the WOW6432Node-redirected view instead and never find it).
  SetRegView 64
  ClearErrors
  ReadRegDWORD $1 HKLM "SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\X64" "Installed"
  IfErrors 0 +2
    StrCpy $1 "0"
  IntCmp $1 1 vcredist_done

  ; Not bundled in a dev/debug build (only `tauri build` copies it in via
  ; bundle.resources) — skip quietly rather than erroring, same as this
  ; hook simply not running at all outside a real installer.
  IfFileExists "$INSTDIR\vc_redist.x64.exe" 0 vcredist_done

  ; Needs admin rights regardless of whether this installer itself is
  ; running elevated — Windows will prompt for that separately if needed,
  ; same as it would for a manual double-click of this exe.
  ExecWait '"$INSTDIR\vc_redist.x64.exe" /install /quiet /norestart' $2
  ; 0 = installed now, 3010 = installed but needs a reboot to finish,
  ; 1638 = a newer version is already present — all three are fine.
  IntCmp $2 0 vcredist_done
  IntCmp $2 3010 vcredist_done
  IntCmp $2 1638 vcredist_done
  MessageBox MB_ICONEXCLAMATION|MB_OK "Não foi possível instalar automaticamente o Visual C++ Redistributable (código $2). O programa pode não abrir sem ele — você pode tentar instalá-lo manualmente rodando:$\n$INSTDIR\vc_redist.x64.exe"

  vcredist_done:
  Pop $2
  Pop $1
!macroend
