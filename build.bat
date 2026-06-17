@echo off
REM Builds the server with the whisper.cpp/whisper-rs environment (see build.ps1).
REM Bypasses PowerShell ExecutionPolicy so no system change is needed.
REM Usage:  build.bat            (release build of server_voice_s3r)
REM         build.bat --bin pc_speaker
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
