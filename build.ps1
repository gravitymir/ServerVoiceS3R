# build.ps1 - builds the server with the environment whisper.cpp/whisper-rs needs.
# whisper-rs compiles whisper.cpp (needs CMake on PATH) and runs bindgen (needs the
# MSVC + Windows SDK headers, which we import from vcvars and pass to clang).
# Usage:  .\build.ps1                  (release build of server_voice_s3r)
#         .\build.ps1 --bin pc_speaker
$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot  # run cargo in the project dir regardless of cwd

$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found - install VS 2022 Build Tools (C++)." }

# Import the MSVC dev environment (INCLUDE/LIB/LIBPATH).
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
  if ($_ -match '^(INCLUDE|LIB|LIBPATH)=(.*)$') { Set-Item -Path "Env:$($matches[1])" -Value $matches[2] }
}

# Point bindgen's clang at the MSVC/SDK headers and the host target.
$isys = ($env:INCLUDE -split ';' | Where-Object { $_ } | ForEach-Object { "-isystem `"$_`"" }) -join ' '
$env:BINDGEN_EXTRA_CLANG_ARGS = "--target=x86_64-pc-windows-msvc $isys"

# CMake (installed via pip, user scope).
$cmakeDir = "C:\Users\gravi\AppData\Roaming\Python\Python314\Scripts"
if (Test-Path (Join-Path $cmakeDir "cmake.exe")) { $env:PATH = "$cmakeDir;" + $env:PATH }

if ($args.Count -gt 0) {
  cargo build --release @args
} else {
  cargo build --release --bin server_voice_s3r
}
