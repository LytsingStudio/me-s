Unicode True
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "StrFunc.nsh"
!include "WinMessages.nsh"
${Using:StrFunc} StrStr
${Using:StrFunc} UnStrRep

!ifndef VERSION
  !error "VERSION is required"
!endif
!ifndef ME_S
  !error "ME_S is required"
!endif
!ifndef ME_GATEWAY
  !error "ME_GATEWAY is required"
!endif
!ifndef ME_CLIENT
  !error "ME_CLIENT is required"
!endif
!ifndef OUTPUT
  !error "OUTPUT is required"
!endif
!ifndef ICON
  !error "ICON is required"
!endif

Name "ME"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\ME"
InstallDirRegKey HKCU "Software\Lytsing Studio\ME" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Icon "${ICON}"
UninstallIcon "${ICON}"
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "ME"
VIAddVersionKey "CompanyName" "Lytsing Studio"
VIAddVersionKey "FileDescription" "ME Installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

!define MUI_ABORTWARNING
!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

Function AddInstallDirToPath
  ReadRegStr $0 HKCU "Environment" "Path"
  StrCpy $1 ";$0;"
  ${StrStr} $2 "$1" ";$INSTDIR;"
  StrCmp $2 "" 0 done
  StrCmp $0 "" 0 append
  StrCpy $0 "$INSTDIR"
  Goto write
append:
  StrCpy $0 "$0;$INSTDIR"
write:
  WriteRegExpandStr HKCU "Environment" "Path" "$0"
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
done:
FunctionEnd

Function un.RemoveInstallDirFromPath
  ReadRegStr $0 HKCU "Environment" "Path"
  StrCpy $1 ";$0;"
  ${UnStrRep} $1 "$1" ";$INSTDIR;" ";"
  StrLen $2 $1
  IntCmp $2 1 empty trim
trim:
  StrCpy $1 $1 "" 1
  StrLen $2 $1
  IntOp $2 $2 - 1
  StrCpy $1 $1 $2
  Goto write
empty:
  StrCpy $1 ""
write:
  WriteRegExpandStr HKCU "Environment" "Path" "$1"
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
FunctionEnd

Section "ME" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File /oname=me-s.exe "${ME_S}"
  File /oname=me-gateway.exe "${ME_GATEWAY}"
  File /oname=me-client.exe "${ME_CLIENT}"
  WriteUninstaller "$INSTDIR\Uninstall ME.exe"
  WriteRegStr HKCU "Software\Lytsing Studio\ME" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME" "DisplayName" "ME"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME" "Publisher" "Lytsing Studio"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME" "DisplayIcon" "$INSTDIR\me-client.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME" "UninstallString" '"$INSTDIR\Uninstall ME.exe"'
  CreateDirectory "$SMPROGRAMS\ME"
  CreateShortcut "$SMPROGRAMS\ME\ME Client.lnk" "$INSTDIR\me-client.exe"
  CreateShortcut "$SMPROGRAMS\ME\Uninstall ME.lnk" "$INSTDIR\Uninstall ME.exe"
  Call AddInstallDirToPath
SectionEnd

Section "Uninstall"
  Call un.RemoveInstallDirFromPath
  Delete "$SMPROGRAMS\ME\ME Client.lnk"
  Delete "$SMPROGRAMS\ME\Uninstall ME.lnk"
  RMDir "$SMPROGRAMS\ME"
  Delete "$INSTDIR\me-s.exe"
  Delete "$INSTDIR\me-gateway.exe"
  Delete "$INSTDIR\me-client.exe"
  Delete "$INSTDIR\Uninstall ME.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME"
  DeleteRegKey HKCU "Software\Lytsing Studio\ME"
SectionEnd
