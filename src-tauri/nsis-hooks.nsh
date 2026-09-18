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
!macroend
