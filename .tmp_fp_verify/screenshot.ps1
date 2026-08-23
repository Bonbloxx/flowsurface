Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class NativeWin {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

$proc = Get-Process -Id 20056 -ErrorAction Stop
if (-not $proc) { throw "flowsurface not running" }
$hwnd = $proc.MainWindowHandle
if ($hwnd -eq [IntPtr]::Zero) { throw "no main window handle" }

$rect = New-Object NativeWin+RECT
[NativeWin]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
if ($w -le 0 -or $h -le 0) { throw "invalid window rect $w x $h" }

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$printed = [NativeWin]::PrintWindow($hwnd, $hdc, 2)
$g.ReleaseHdc($hdc)
if (-not $printed) { throw "PrintWindow failed" }
$out = "C:\Users\verne\Documents\Projects\flowsurface\.tmp_fp_verify\window.png"
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)

# Bottom-right quadrant is the footprint pane (vertical 0.5, then horizontal 0.5).
$padX = [int]($w * 0.50)
$padY = [int]($h * 0.50)
$fw = $w - $padX - 8
$fh = $h - $padY - 8
$crop = $bmp.Clone((New-Object System.Drawing.Rectangle($padX, $padY, $fw, $fh)), $bmp.PixelFormat)
$cropOut = "C:\Users\verne\Documents\Projects\flowsurface\.tmp_fp_verify\footprint.png"
$crop.Save($cropOut, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose(); $crop.Dispose()
Write-Output "saved $out ($w x $h)"
Write-Output "saved $cropOut ($fw x $fh) from ($padX,$padY)"
