; NSIS installer hooks (tauri.conf.json bundle.windows.nsis.installerHooks).
;
; The uninstaller's "Delete the application data" checkbox makes Tauri's template remove
; %APPDATA%\<bundle id> and %LOCALAPPDATA%\<bundle id>. Tincan keeps nothing there: every
; profile (identity key, message history, downloaded files and clips, WebView2 data) is in
; %APPDATA%\Tincan\profiles\<name>. So the ticked box also removes %APPDATA%\Tincan.
; An update (/UPDATE) never removes anything.
;
; The template inserts this macro in Section Uninstall, right after its own app data removal
; and after CheckIfAppIsRunning closed Tincan. $DeleteAppDataCheckboxState and $UpdateMode
; are the template's variables; LogicLib is already included.

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    ${If} "$APPDATA" != ""
      RmDir /r "$APPDATA\Tincan"
      ; A WebView2 helper process can hold files in profiles\<name>\webview for a moment
      ; after Tincan itself has exited: try once more.
      ${If} ${FileExists} "$APPDATA\Tincan\*.*"
        Sleep 2000
        RmDir /r "$APPDATA\Tincan"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend
