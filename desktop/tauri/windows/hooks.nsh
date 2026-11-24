; Irelia Companion 1.0 installer hooks (Tauri NSIS).
;
; 0.x was an Electron app installed per user by electron-builder (appId com.ireliacompanion.app, so its uninstall key
; is the GUID below). Installing 1.0 removes it first, quietly, the way its own updates did: its uninstaller keeps
; %APPDATA%\irelia-companion (config.json, data\, evals\, backups), which 1.0 opens as it is.

!define IRELIA_0X_GUID "5990d65b-2c7c-593e-a211-7d983f7fabd9"

!macro NSIS_HOOK_PREINSTALL
  Push $R0
  Push $R1
  ReadRegStr $R0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${IRELIA_0X_GUID}" "UninstallString"
  ${If} $R0 != ""
    DetailPrint "Removing Irelia Companion 0.x (your data and settings stay)..."
    ; A 0.x still running (or its middleware) would keep its files.
    nsExec::Exec 'taskkill /F /T /IM IreliaCompanion.exe'
    Pop $R1
    Sleep 1000
    ReadRegStr $R1 HKCU "Software\${IRELIA_0X_GUID}" "InstallLocation"
    ${If} $R1 != ""
      ; _?= runs the uninstaller in place, so ExecWait waits for it.
      ExecWait '$R0 /S _?=$R1'
      Delete "$R1\Uninstall Irelia Companion.exe"
      RMDir "$R1"
    ${Else}
      ExecWait '$R0 /S'
    ${EndIf}
  ${EndIf}
  Pop $R1
  Pop $R0
!macroend
