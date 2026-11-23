# Answers the native "Open" file dialog of one Tincan process without keyboard or mouse
# input: waits for a top-level #32770 window of process $ProcId, sets the file name box
# (control 1148 -> ComboBox -> Edit) with WM_SETTEXT and presses Open (IDOK) with WM_COMMAND.
# (UI Automation sees these classic controls only as panes here, so plain Win32 is used.)
param([Parameter(Mandatory)][int]$ProcId, [Parameter(Mandatory)][string]$Path, [int]$TimeoutMs = 20000)

$ErrorActionPreference = 'Stop'
Add-Type -Namespace E2E -Name Win -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowEx(System.IntPtr parent, System.IntPtr after, string cls, string title);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr hwnd, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr hwnd);
[DllImport("user32.dll")] public static extern System.IntPtr GetDlgItem(System.IntPtr dlg, int id);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr SendMessage(System.IntPtr hwnd, uint msg, System.IntPtr w, string l);
[DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr hwnd, uint msg, System.IntPtr w, System.IntPtr l);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(System.IntPtr hwnd, System.Text.StringBuilder sb, int max);
'@

function Find-Dialog {
  $h = [IntPtr]::Zero
  while ($true) {
    $h = [E2E.Win]::FindWindowEx([IntPtr]::Zero, $h, '#32770', [NullString]::Value)
    if ($h -eq [IntPtr]::Zero) { return $null }
    [uint32]$owner = 0
    [void][E2E.Win]::GetWindowThreadProcessId($h, [ref]$owner)
    if ($owner -eq $ProcId -and [E2E.Win]::IsWindowVisible($h)) { return $h }
  }
}

$deadline = (Get-Date).AddMilliseconds($TimeoutMs)
$dlg = $null
$edit = [IntPtr]::Zero
while ($true) {
  $dlg = Find-Dialog
  if ($dlg) {
    $combo = [E2E.Win]::GetDlgItem($dlg, 1148)
    if ($combo -ne [IntPtr]::Zero) {
      $inner = [E2E.Win]::FindWindowEx($combo, [IntPtr]::Zero, 'ComboBox', [NullString]::Value)
      if ($inner -ne [IntPtr]::Zero) { $edit = [E2E.Win]::FindWindowEx($inner, [IntPtr]::Zero, 'Edit', [NullString]::Value) }
    }
    if ($edit -ne [IntPtr]::Zero) { break }
  }
  if ((Get-Date) -gt $deadline) { Write-Output ('ERR ' + $(if ($dlg) { 'no file name box' } else { 'no dialog' })); exit 2 }
  Start-Sleep -Milliseconds 150
}

$WM_SETTEXT = 0x000C
$WM_COMMAND = 0x0111
[void][E2E.Win]::SendMessage($edit, $WM_SETTEXT, [IntPtr]::Zero, $Path)
$ok = [E2E.Win]::GetDlgItem($dlg, 1)
$title = New-Object System.Text.StringBuilder 128
[void][E2E.Win]::GetWindowText($dlg, $title, 128)
# IDOK = 1, BN_CLICKED = 0 in the high word
[void][E2E.Win]::PostMessage($dlg, $WM_COMMAND, [IntPtr]1, $ok)

while ([E2E.Win]::IsWindowVisible($dlg)) {
  if ((Get-Date) -gt $deadline) { Write-Output 'ERR dialog stayed open'; exit 3 }
  Start-Sleep -Milliseconds 100
}
Write-Output "OK $title"
