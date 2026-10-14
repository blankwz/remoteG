param(
    [int]$Width = 800,
    [int]$Height = 600
)
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class W {
    [DllImport("user32.dll")]
    public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
}
'@
$p = Get-Process clarity-bench
$ok = [W]::SetWindowPos($p.MainWindowHandle, [IntPtr]::Zero, 100, 100, $Width, $Height, 0x14)
Write-Output "resize to ${Width}x${Height}: $ok"
