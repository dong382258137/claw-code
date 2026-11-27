# package-release.ps1 — 组装 Windows 发布目录：claw.exe + onnxruntime.dll。
#
# 前端资源（web/dist/index.html）在编译期已内嵌进 claw.exe,无需单独拷贝。
# onnxruntime.dll 不内嵌:与主程序并排分发;缺失时服务仍可启动,
# 语义检索自动降级为关键词匹配（设置页会提示）。
#
# 用法:
#   powershell -ExecutionPolicy Bypass -File scripts/package-release.ps1
#   powershell -ExecutionPolicy Bypass -File scripts/package-release.ps1 -SkipBuild
#
# DLL 来源（任选其一）:
#   1. 已放在 rust/target/release/onnxruntime.dll
#   2. 设置 ORT_DYLIB_PATH 指向任意 onnxruntime.dll(>=1.24)
#   3. 从 PyPI onnxruntime==1.24.2 的 win_amd64 wheel 提取 capi/onnxruntime.dll
param(
    [switch]$SkipBuild,
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$rustDir = Join-Path $root "rust"
$releaseDir = Join-Path $rustDir "target\release"
$exe = Join-Path $releaseDir "claw.exe"

if (-not $SkipBuild) {
    Write-Host "▶ cargo build --release -p claw-server"
    & cargo build --release -p claw-server --manifest-path (Join-Path $rustDir "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "release 构建失败" }
}
if (-not (Test-Path $exe)) { throw "未找到 $exe,请先构建 release" }

# 版本号取自 workspace.package.version,避免解析二进制输出。
$manifest = Get-Content (Join-Path $rustDir "Cargo.toml") -Raw
$version = [regex]::Match($manifest, 'version\s*=\s*"([^"]+)"').Groups[1].Value
$target = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
$package = Join-Path $target "claw-$version-win-x64"
if (Test-Path $package) { Remove-Item -Recurse -Force $package }
New-Item -ItemType Directory -Force $package | Out-Null

Copy-Item $exe $package

$dll = Join-Path $releaseDir "onnxruntime.dll"
if (-not (Test-Path $dll)) {
    if ($env:ORT_DYLIB_PATH -and (Test-Path $env:ORT_DYLIB_PATH)) {
        $dll = $env:ORT_DYLIB_PATH
    } else {
        throw "缺少 onnxruntime.dll(>=1.24)。请放到 $releaseDir 或设置 ORT_DYLIB_PATH 后重试。"
    }
}
Copy-Item $dll $package
$shared = Join-Path (Split-Path $dll -Parent) "onnxruntime_providers_shared.dll"
if (Test-Path $shared) { Copy-Item $shared $package }

$readme = @"
claw $version (win-x64)
========================

启动:
  claw.exe serve --port 8787
  浏览器打开终端里打印的 http://127.0.0.1:8787/?t=...

onnxruntime.dll 的作用:
  本地语义检索（bge-small-zh-v1.5,512 维）的推理运行时。
  必须与 claw.exe 放在同一目录,或通过 ORT_DYLIB_PATH 指定路径。
  首次检索会把 ONNX 模型下载到 %USERPROFILE%\.cache\fastembed
  （可用 FASTEMBED_CACHE_DIR 或 HF_HOME 覆盖）。

缺少 onnxruntime.dll 时:
  服务仍可正常启动,语义检索自动降级为关键词匹配;
  Web 设置页「关于 → 语义检索」会显示「关键词降级」及原因。
"@
Set-Content -Path (Join-Path $package "README.txt") -Value $readme -Encoding UTF8

$zip = "$package.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$package\*" -DestinationPath $zip
Write-Host "✓ 发布目录: $package"
Write-Host "✓ 压缩包:   $zip"

