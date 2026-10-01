# 生成 Toocode 的应用图标源图（1024x1024 PNG）
#
# 用法：
#   & .\scripts\make-logo.ps1          # 生成 ..\logo.png
#   npm run tauri icon logo.png        # 再展开成全套图标（.ico / .icns / 各种尺寸）
#
# ★ 为什么用 .NET 画，而不是用浏览器渲染 SVG 再截图：
#   Playwright 在 VS Code 的内嵌浏览器里截出来的图和实际尺寸对不上
#   （截到的是放大的一角）。System.Drawing 完全可控，也不依赖任何外部工具。
#
# ★ 图形：圆角方形底 + 一个「>」加一条下划线（终端提示符）。
#   选它的理由：两笔就画完，小尺寸下也认得出来，而且是单色的。

Add-Type -AssemblyName System.Drawing

$size   = 1024
$radius = 224    # 圆角半径，约 22% —— Windows / macOS 图标的惯例比例
$bgColor = [System.Drawing.Color]::FromArgb(27, 31, 36)     # #1b1f24 深蓝灰
$fgColor = [System.Drawing.Color]::FromArgb(230, 237, 243)  # #e6edf3 柔和的近白
$stroke  = 84

$bmp = New-Object System.Drawing.Bitmap($size, $size)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias

# ---- 圆角方形底 ----
$path = New-Object System.Drawing.Drawing2D.GraphicsPath
$d = $radius * 2
$path.AddArc(0, 0, $d, $d, 180, 90)
$path.AddArc($size - $d, 0, $d, $d, 270, 90)
$path.AddArc($size - $d, $size - $d, $d, $d, 0, 90)
$path.AddArc(0, $size - $d, $d, $d, 90, 90)
$path.CloseFigure()
$bg = New-Object System.Drawing.SolidBrush($bgColor)
$g.FillPath($bg, $path)

# ---- 符号：">" + 下划线 ----
$pen = New-Object System.Drawing.Pen($fgColor, $stroke)
$pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$pen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round

$g.DrawLines($pen, @(
  (New-Object System.Drawing.Point(324, 356)),
  (New-Object System.Drawing.Point(476, 512)),
  (New-Object System.Drawing.Point(324, 668))
))
$g.DrawLine($pen, 528, 668, 688, 668)

$g.Dispose()

$out = Join-Path (Split-Path $PSScriptRoot -Parent) "logo.png"
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$w = $bmp.Width
$h = $bmp.Height
$bmp.Dispose()

"已生成 $out"
"尺寸 $w x $h，$((Get-Item $out).Length) 字节"
"接下来跑：npm run tauri icon logo.png"
