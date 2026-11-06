; SP-404 Drive's Windows installer: one .exe holding everything the drive needs.
;
; - The three programs (sp404drive, sp404drive-host, sp404drive-tray), built with the C
;   runtime linked in, so no Visual C++ Redistributable is needed.
; - WinFsp's own signed installer, run silently when WinFsp is missing or older than the
;   one carried here. Uninstalling SP-404 Drive leaves WinFsp: other programs may use it.
;
; Built by build-installer.ps1, which supplies the defines below. Not signed yet; when
; it is, the uninstaller needs a two-pass build: one pass writes Uninstall.exe, which is
; signed, then the second pass embeds the signed copy.

Unicode True
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "WordFunc.nsh"

!ifndef Version
  !error "Version must be supplied by build-installer.ps1."
!endif
!ifndef BinDir
  !error "BinDir must point to the release build (target\release)."
!endif
!ifndef OutDir
  !error "OutDir must be supplied by build-installer.ps1."
!endif
!ifndef WinFspMsi
  !error "WinFspMsi must point to WinFsp's installer."
!endif
!ifndef WinFspVersion
  !error "WinFspVersion must be supplied by build-installer.ps1."
!endif
!ifndef IconFile
  !error "IconFile must be supplied by build-installer.ps1."
!endif
!ifndef LicenceFile
  !error "LicenceFile must be supplied by build-installer.ps1."
!endif
!ifndef NoticesFile
  !error "NoticesFile must be supplied by build-installer.ps1."
!endif

!define ProductName "SP-404 Drive"
!define ProductId "SP404Drive"
!define Publisher "Polecat Plugins"
!define UninstallKey "Software\Microsoft\Windows\CurrentVersion\Uninstall\${ProductId}"
!define Tray "sp404drive-tray.exe"
; The licence texts of the crates compiled into the programs (see THIRD-PARTY-NOTICES.txt).
!define LicencesDir "${__FILEDIR__}\..\licences"

Name "${ProductName} ${Version}"
OutFile "${OutDir}\SP-404-Drive-${Version}-windows-x64-setup.exe"
RequestExecutionLevel admin
InstallDir "$PROGRAMFILES64\Polecat Plugins\SP-404 Drive"
InstallDirRegKey HKLM "${UninstallKey}" "InstallLocation"
BrandingText "${Publisher}"
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${Version}.0"
VIAddVersionKey "ProductName" "${ProductName}"
VIAddVersionKey "CompanyName" "${Publisher}"
VIAddVersionKey "FileDescription" "${ProductName} installer"
VIAddVersionKey "FileVersion" "${Version}"
VIAddVersionKey "ProductVersion" "${Version}"
VIAddVersionKey "LegalCopyright" "Copyright © 2026 polecatplugins.com. GPL-3.0-only."

!define MUI_ICON "${IconFile}"
!define MUI_UNICON "${IconFile}"
!define MUI_ABORTWARNING

!define MUI_WELCOMEPAGE_TEXT "This installs ${ProductName} ${Version}, which shows a Roland SP-404MKII as a drive in File Explorer.$\r$\n$\r$\nIt includes WinFsp, the free file-system driver the drive runs on, and installs it if this PC doesn't have it yet.$\r$\n$\r$\nQuit Roland's SP-404MKII app before using the drive: only one program can use the unit at a time."
!insertmacro MUI_PAGE_WELCOME
!define MUI_LICENSEPAGE_TEXT_TOP "Please read the notice at the top before installing."
!insertmacro MUI_PAGE_LICENSE "${LicenceFile}"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Start ${ProductName}"
!define MUI_FINISHPAGE_RUN_FUNCTION StartTray
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${ProductName} needs 64-bit Windows."
    Abort
  ${EndIf}
  ${IfNot} ${AtLeastWin10}
    MessageBox MB_ICONSTOP "${ProductName} needs Windows 10 or later."
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; Unelevated, through Explorer, as if from the Start menu: a drive letter made by an
; elevated program is invisible to the user's own Explorer, and an elevated tray app
; couldn't hear from the drive.
Function StartTray
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${Tray}"'
FunctionEnd

; Install WinFsp unless this PC has it at this version or newer.
Function InstallWinFsp
  ; WinFsp records its folder in the 32-bit view of the registry.
  SetRegView 32
  ReadRegStr $0 HKLM "SOFTWARE\WinFsp" "InstallDir"
  SetRegView 64
  ${If} $0 != ""
    ClearErrors
    GetDLLVersion "$0bin\winfsp-x64.dll" $1 $2
    ${IfNot} ${Errors}
      IntOp $3 $1 >> 16
      IntOp $4 $1 & 0xFFFF
      IntOp $5 $2 >> 16
      ; 0: the same, 1: installed is newer, 2: ours is newer.
      ${VersionCompare} "$3.$4.$5" "${WinFspVersion}" $6
      ${If} $6 != 2
        DetailPrint "WinFsp $3.$4.$5 is already installed."
        Return
      ${EndIf}
      DetailPrint "WinFsp $3.$4.$5 is installed; updating it to ${WinFspVersion}."
    ${EndIf}
  ${EndIf}
  SetOutPath "$PLUGINSDIR"
  File "/oname=$PLUGINSDIR\winfsp.msi" "${WinFspMsi}"
  DetailPrint "Installing WinFsp ${WinFspVersion}..."
  ExecWait '"$SYSDIR\msiexec.exe" /i "$PLUGINSDIR\winfsp.msi" /qn /norestart' $0
  ${If} $0 == 3010
    SetRebootFlag true
  ${ElseIf} $0 != 0
    Abort "WinFsp couldn't be installed (msiexec exit code $0)."
  ${EndIf}
FunctionEnd

Section "${ProductName}" SecMain
  SetShellVarContext all

  ; A copy already running, or a drive still mounted, holds the files: the new tray app
  ; asks the running one to eject and quit (it waits until it has), then any drive mounted
  ; with the command is unmounted.
  InitPluginsDir
  File "/oname=$PLUGINSDIR\${Tray}" "${BinDir}\${Tray}"
  ExecWait '"$PLUGINSDIR\${Tray}" --quit'
  ${If} ${FileExists} "$INSTDIR\sp404drive.exe"
    nsExec::Exec '"$INSTDIR\sp404drive.exe" unmount'
    Pop $0
  ${EndIf}

  Call InstallWinFsp

  SetOutPath "$INSTDIR"
  File "${BinDir}\sp404drive.exe"
  File "${BinDir}\sp404drive-host.exe"
  File "${BinDir}\${Tray}"
  File "/oname=LICENSE.txt" "${LicenceFile}"
  File "/oname=THIRD-PARTY-NOTICES.txt" "${NoticesFile}"
  SetOutPath "$INSTDIR\licences"
  File "${LicencesDir}\Apache-2.0.txt"
  File "${LicencesDir}\MIT-strsim.txt"
  File "${LicencesDir}\Unicode-3.0-unicode-normalization.txt"
  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; The command, for every user's terminal.
  ExecWait '"$INSTDIR\${Tray}" --add-to-path' $0
  ${If} $0 != 0
    DetailPrint "Couldn't add $INSTDIR to the PATH; use the full path to sp404drive.exe."
  ${EndIf}

  CreateShortcut "$SMPROGRAMS\${ProductName}.lnk" "$INSTDIR\${Tray}"

  WriteRegStr HKLM "${UninstallKey}" "DisplayName" "${ProductName}"
  WriteRegStr HKLM "${UninstallKey}" "DisplayVersion" "${Version}"
  WriteRegStr HKLM "${UninstallKey}" "Publisher" "${Publisher}"
  WriteRegStr HKLM "${UninstallKey}" "URLInfoAbout" "https://polecatplugins.com"
  WriteRegStr HKLM "${UninstallKey}" "DisplayIcon" "$INSTDIR\${Tray}"
  WriteRegStr HKLM "${UninstallKey}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UninstallKey}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKLM "${UninstallKey}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKLM "${UninstallKey}" "NoModify" 1
  WriteRegDWORD HKLM "${UninstallKey}" "NoRepair" 1
  ; In KB, for Installed apps.
  WriteRegDWORD HKLM "${UninstallKey}" "EstimatedSize" 7000
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  ExecWait '"$INSTDIR\${Tray}" --quit'
  nsExec::Exec '"$INSTDIR\sp404drive.exe" unmount'
  Pop $0
  ExecWait '"$INSTDIR\${Tray}" --remove-from-path'

  ; Open at Sign-in, for the user running the uninstaller.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${ProductName}"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${ProductName}"

  Delete "$SMPROGRAMS\${ProductName}.lnk"
  Delete "$INSTDIR\sp404drive.exe"
  Delete "$INSTDIR\sp404drive-host.exe"
  Delete "$INSTDIR\${Tray}"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  Delete "$INSTDIR\licences\Apache-2.0.txt"
  Delete "$INSTDIR\licences\MIT-strsim.txt"
  Delete "$INSTDIR\licences\Unicode-3.0-unicode-normalization.txt"
  RMDir "$INSTDIR\licences"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  ; Only if nothing else of ours is in it.
  RMDir "$PROGRAMFILES64\Polecat Plugins"
  DeleteRegKey HKLM "${UninstallKey}"
  ; WinFsp stays: other programs may use it. It has its own entry in Installed apps.
SectionEnd
