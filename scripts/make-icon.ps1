<#
.SYNOPSIS
    Gera assets/app.png (256 px) e assets/app.ico (16 a 256 px) a partir de desenho vetorial.

.DESCRIPTION
    O ícone repete a marca da barra lateral: um traço de sinal sobre grafite,
    terminando no ponto luminoso que os gráficos do app usam para o valor atual.
    Tamanhos pequenos são redesenhados (não reduzidos) com traço mais grosso,
    senão a linha some em 16 px. Rodar de novo só se o desenho mudar.
#>
Add-Type -AssemblyName System.Drawing

$assets = Join-Path $PSScriptRoot '..\assets' | Resolve-Path
$graphite0 = [System.Drawing.Color]::FromArgb(255, 0x0E, 0x10, 0x13)
$graphite2 = [System.Drawing.Color]::FromArgb(255, 0x1E, 0x23, 0x29)
$cpu = [System.Drawing.Color]::FromArgb(255, 0x4C, 0xC2, 0xFF)

function New-RoundedRect([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    $path
}

function Render([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    $s = $size / 256.0
    $inset = [Math]::Max(0.5, 8 * $s)
    $box = New-RoundedRect $inset $inset ($size - 2 * $inset) ($size - 2 * $inset) (56 * $s)
    $bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.PointF(0, 0)), (New-Object System.Drawing.PointF(0, $size)), $graphite2, $graphite0)
    $g.FillPath($bg, $box)
    if ($size -ge 32) {
        $edge = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(40, 255, 255, 255), [Math]::Max(1, 3 * $s))
        $g.DrawPath($edge, $box)
    }

    # Traço: plano, pico, vale, assenta e segue até o ponto de cabeça.
    $stroke = if ($size -le 24) { 26 } elseif ($size -le 48) { 20 } else { 15 }
    $pts = @(
        @(48, 140), @(92, 140), @(116, 70), @(146, 190), @(170, 126), @(206, 126)
    ) | ForEach-Object { New-Object System.Drawing.PointF(($_[0] * $s), ($_[1] * $s)) }
    $pen = New-Object System.Drawing.Pen($cpu, ($stroke * $s))
    $pen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawLines($pen, [System.Drawing.PointF[]]$pts)

    $head = $pts[-1]
    if ($size -ge 32) {
        $glow = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(60, $cpu))
        $gr = 30 * $s
        $g.FillEllipse($glow, $head.X - $gr, $head.Y - $gr, 2 * $gr, 2 * $gr)
    }
    $dot = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 0xE8, 0xF7, 0xFF))
    $dr = [Math]::Max(1.5, 14 * $s)
    $g.FillEllipse($dot, $head.X - $dr, $head.Y - $dr, 2 * $dr, 2 * $dr)

    $g.Dispose()
    $bmp
}

function PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $ms.ToArray()
}

$sizes = 16, 24, 32, 48, 64, 128, 256
# O PowerShell desenrola arrays na saída das funções; o cast remonta o byte[].
$images = New-Object System.Collections.Generic.List[byte[]]
foreach ($size in $sizes) { $images.Add([byte[]](PngBytes (Render $size))) }

[System.IO.File]::WriteAllBytes((Join-Path $assets 'app.png'), $images[-1])

# ICO com entradas PNG (aceito desde o Windows Vista).
$ms = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter($ms)
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$images[$i].Length); $w.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $w.Write($img) }
$w.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $assets 'app.ico'), $ms.ToArray())
Write-Host "Gerados: $assets\app.png e app.ico"
