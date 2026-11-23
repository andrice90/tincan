# A window to screen-share in the end-to-end run: four saturated quadrants (red, green,
# blue, yellow) and a small white square that moves, so Windows.Graphics.Capture keeps
# delivering frames. Title: tincan-e2e-pattern. Closes itself after $MaxSeconds.
param([int]$MaxSeconds = 900)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$form = New-Object System.Windows.Forms.Form
$form.Text = 'tincan-e2e-pattern'
$form.ClientSize = New-Object System.Drawing.Size(640, 480)
$form.StartPosition = 'Manual'
$form.Location = New-Object System.Drawing.Point(60, 60)
$form.FormBorderStyle = 'FixedSingle'
$form.MaximizeBox = $false
$form.BackColor = [System.Drawing.Color]::Black

$colors = @(
  @(0, 0, [System.Drawing.Color]::FromArgb(255, 0, 0)),
  @(320, 0, [System.Drawing.Color]::FromArgb(0, 255, 0)),
  @(0, 240, [System.Drawing.Color]::FromArgb(0, 0, 255)),
  @(320, 240, [System.Drawing.Color]::FromArgb(255, 255, 0))
)
foreach ($c in $colors) {
  $p = New-Object System.Windows.Forms.Panel
  $p.Location = New-Object System.Drawing.Point($c[0], $c[1])
  $p.Size = New-Object System.Drawing.Size(320, 240)
  $p.BackColor = $c[2]
  $form.Controls.Add($p)
}

$dot = New-Object System.Windows.Forms.Panel
$dot.Size = New-Object System.Drawing.Size(24, 24)
$dot.BackColor = [System.Drawing.Color]::White
$form.Controls.Add($dot)
$dot.BringToFront()

$script:t = 0
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 100
$timer.Add_Tick({
  $script:t++
  $x = 308 + [int](40 * [Math]::Cos($script:t / 5.0))
  $y = 228 + [int](40 * [Math]::Sin($script:t / 5.0))
  $dot.Location = New-Object System.Drawing.Point($x, $y)
  if ($script:t -gt ($MaxSeconds * 10)) { $form.Close() }
})
$timer.Start()

$form.Add_Shown({ $form.Activate() })
[void]$form.ShowDialog()
