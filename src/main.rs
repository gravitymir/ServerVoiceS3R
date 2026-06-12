//! ATOM VoiceS3R PC server.
//!
//! Protocol (raw TCP, one connection per utterance):
//!   - Device connects, streams 16 kHz / mono / 16-bit LE PCM, then half-closes
//!     its write side (EOF) to mark end of utterance.
//!   - We turn it into a spoken reply and stream 16 kHz mono PCM back, then close.
//!
//! Modes (env `MODE`, default `windows`; `LOOPBACK=1` forces loopback):
//!   - `loopback` : echo the recorded audio back (no AI, no keys).
//!   - `windows`  : Windows System.Speech STT + `claude` CLI reply + SAPI TTS.
//!   - `openai`   : OpenAI Whisper + Chat + TTS (needs OPENAI_API_KEY).
//!
//! Other env: PORT (9000), CHAT_MODEL, TTS_MODEL, TTS_VOICE, STT_MODEL.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

const DEVICE_RATE: u32 = 16_000; // device PCM sample rate
const TTS_RATE: u32 = 24_000; // OpenAI TTS pcm sample rate

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Config {
    api_key: String,
    port: u16,
    chat_model: String,
    tts_model: String,
    tts_voice: String,
    stt_model: String,
    mode: String,
    stt_script: PathBuf,
    tts_script: PathBuf,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() -> Result<()> {
    let mode = if std::env::var("LOOPBACK").is_ok() {
        "loopback".to_string()
    } else {
        env_or("MODE", "windows").to_lowercase()
    };

    let api_key = if mode == "openai" {
        std::env::var("OPENAI_API_KEY")
            .map_err(|_| anyhow!("MODE=openai needs OPENAI_API_KEY"))?
    } else {
        String::new()
    };

    let (stt_script, tts_script) = write_scripts()?;

    let cfg = Config {
        api_key,
        port: env_or("PORT", "9000").parse().unwrap_or(9000),
        chat_model: env_or("CHAT_MODEL", "gpt-4o-mini"),
        tts_model: env_or("TTS_MODEL", "gpt-4o-mini-tts"),
        tts_voice: env_or("TTS_VOICE", "alloy"),
        stt_model: env_or("STT_MODEL", "whisper-1"),
        mode,
        stt_script,
        tts_script,
    };

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).with_context(|| format!("bind {addr}"))?;
    log(&format!("MODE = {}", cfg.mode));
    match cfg.mode.as_str() {
        "windows" => log("STT: Windows System.Speech  |  reply: claude CLI  |  TTS: Windows SAPI"),
        "openai" => log(&format!(
            "OpenAI: stt={} chat={} tts={} voice={}",
            cfg.stt_model, cfg.chat_model, cfg.tts_model, cfg.tts_voice
        )),
        "loopback" => log("echoing recorded audio back (no AI)"),
        other => log(&format!("WARNING: unknown MODE '{other}'")),
    }
    log(&format!("listening on {addr}"));
    log("waiting for the ATOM VoiceS3R to connect (hold its button to talk)...");

    let cfg = std::sync::Arc::new(cfg);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    log(&format!("── connection from {peer} ──"));
                    if let Err(e) = handle(s, &cfg) {
                        log(&format!("[error] {e:#}"));
                    }
                    log(&format!("── done with {peer} ──"));
                });
            }
            Err(e) => log(&format!("[accept error] {e}")),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, cfg: &Config) -> Result<()> {
    let t0 = Instant::now();

    // 1. Receive the whole utterance (until the device half-closes).
    let mut pcm = Vec::new();
    stream.read_to_end(&mut pcm).context("reading utterance PCM")?;
    let secs = pcm.len() as f32 / (DEVICE_RATE as f32 * 2.0);
    log(&format!(
        "[recv] {} bytes (~{:.1}s of 16kHz mono) in {:?}",
        pcm.len(),
        secs,
        t0.elapsed()
    ));
    if pcm.len() < 4000 {
        log("[skip] utterance too short");
        return Ok(());
    }

    // 2. Produce the response audio according to the mode.
    let out_pcm = match cfg.mode.as_str() {
        "loopback" => {
            log(&format!("[loopback] echoing {} bytes", pcm.len()));
            pcm.clone()
        }
        "windows" => windows_brain(&pcm, cfg)?,
        "openai" => openai_brain(&pcm, cfg)?,
        other => {
            log(&format!("[error] unknown MODE '{other}'"));
            return Ok(());
        }
    };

    if out_pcm.is_empty() {
        log("[skip] no response audio");
        return Ok(());
    }

    // 3. Stream the response back and close.
    stream.write_all(&out_pcm).context("writing response PCM")?;
    stream.flush().ok();
    log(&format!("[done] total {:?}", t0.elapsed()));
    Ok(())
}

// ───────────────────────── Windows-native brain ─────────────────────────

fn windows_brain(pcm: &[u8], cfg: &Config) -> Result<Vec<u8>> {
    let tmp = std::env::temp_dir();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let wav_in = tmp.join(format!("vs3r_in_{id}.wav"));
    let txt = tmp.join(format!("vs3r_reply_{id}.txt"));
    let wav_out = tmp.join(format!("vs3r_out_{id}.wav"));

    std::fs::write(&wav_in, pcm_to_wav(pcm, DEVICE_RATE))?;

    // STT (Windows System.Speech).
    let t = Instant::now();
    let transcript = run_ps(&cfg.stt_script, &["-Wav", path_str(&wav_in)?])?
        .trim()
        .to_string();
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript));
    if transcript.is_empty() {
        cleanup(&[&wav_in, &txt, &wav_out]);
        log("[skip] empty transcript (mic too quiet or not recognized)");
        return Ok(Vec::new());
    }

    // Reply (claude CLI, prompt via stdin to avoid quoting issues).
    let t = Instant::now();
    let prompt = format!(
        "You are a warm, concise voice assistant speaking through a small smart speaker. \
         Answer the user's spoken words directly in 1-2 short sentences of plain speech. \
         Never mention coding, tasks, tools, or that you are an AI/CLI; never use markdown, \
         lists, or emojis. If the words are unclear, make a friendly best guess rather than \
         asking what they meant.\n\nUser said: {transcript}"
    );
    let reply = run_claude(&prompt)?.trim().to_string();
    log(&format!("[llm {:?}] \"{}\"", t.elapsed(), reply));
    if reply.is_empty() {
        cleanup(&[&wav_in, &txt, &wav_out]);
        return Ok(Vec::new());
    }

    // TTS (Windows SAPI -> 16 kHz mono WAV).
    let t = Instant::now();
    std::fs::write(&txt, &reply)?;
    run_ps(
        &cfg.tts_script,
        &["-TextFile", path_str(&txt)?, "-Out", path_str(&wav_out)?],
    )?;
    let wav = std::fs::read(&wav_out)?;
    let out_pcm = wav_to_pcm(&wav);
    log(&format!("[tts {:?}] {} bytes PCM", t.elapsed(), out_pcm.len()));

    cleanup(&[&wav_in, &txt, &wav_out]);
    Ok(out_pcm)
}

fn run_ps(script: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("spawn powershell")?;
    if !out.status.success() {
        return Err(anyhow!(
            "powershell {:?} failed: {}",
            script.file_name().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_claude(prompt: &str) -> Result<String> {
    let mut child = Command::new("cmd")
        .args(["/C", "claude", "-p"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn claude (is the CLI on PATH?)")?;
    child
        .stdin
        .take()
        .context("claude stdin")?
        .write_all(prompt.as_bytes())?;
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str().context("non-UTF8 temp path")
}

fn cleanup(paths: &[&Path]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

/// Write the Windows STT/TTS helper scripts to temp and return their paths.
fn write_scripts() -> Result<(PathBuf, PathBuf)> {
    let dir = std::env::temp_dir();
    let stt = dir.join("vs3r_stt.ps1");
    let tts = dir.join("vs3r_tts.ps1");
    std::fs::write(&stt, STT_PS)?;
    std::fs::write(&tts, TTS_PS)?;
    Ok((stt, tts))
}

const STT_PS: &str = r#"param([Parameter(Mandatory=$true)][string]$Wav)
Add-Type -AssemblyName System.Speech
$rec = New-Object System.Speech.Recognition.SpeechRecognitionEngine
$rec.LoadGrammar((New-Object System.Speech.Recognition.DictationGrammar))
$rec.SetInputToWaveFile($Wav)
$sb = New-Object System.Text.StringBuilder
while ($true) {
  try { $r = $rec.Recognize() } catch { break }
  if ($r -ne $null) { [void]$sb.Append($r.Text); [void]$sb.Append(' ') } else { break }
}
$rec.Dispose()
[Console]::Out.Write($sb.ToString().Trim())
"#;

const TTS_PS: &str = r#"param([Parameter(Mandatory=$true)][string]$TextFile,[Parameter(Mandatory=$true)][string]$Out)
Add-Type -AssemblyName System.Speech
$text = [System.IO.File]::ReadAllText($TextFile)
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000,[System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen,[System.Speech.AudioFormat.AudioChannel]::Mono)
$synth.SetOutputToWaveFile($Out,$fmt)
$synth.Speak($text)
$synth.Dispose()
"#;

// ───────────────────────────── OpenAI brain ─────────────────────────────

fn openai_brain(pcm: &[u8], cfg: &Config) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::new();

    let t = Instant::now();
    let wav = pcm_to_wav(pcm, DEVICE_RATE);
    let transcript = transcribe(&client, cfg, wav)?;
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript.trim()));
    if transcript.trim().is_empty() {
        return Ok(Vec::new());
    }

    let t = Instant::now();
    let reply = chat(&client, cfg, transcript.trim())?;
    log(&format!("[llm {:?}] \"{}\"", t.elapsed(), reply.trim()));

    let t = Instant::now();
    let tts_pcm = synthesize(&client, cfg, reply.trim())?;
    let out_pcm = resample(&tts_pcm, TTS_RATE, DEVICE_RATE);
    log(&format!(
        "[tts {:?}] {} bytes @24k -> {} bytes @16k",
        t.elapsed(),
        tts_pcm.len(),
        out_pcm.len()
    ));
    Ok(out_pcm)
}

fn transcribe(client: &reqwest::blocking::Client, cfg: &Config, wav: Vec<u8>) -> Result<String> {
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let form = reqwest::blocking::multipart::Form::new()
        .text("model", cfg.stt_model.clone())
        .text("response_format", "json")
        .part("file", part);

    let resp = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&cfg.api_key)
        .multipart(form)
        .send()
        .context("whisper request")?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(anyhow!("whisper {status}: {body}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    Ok(v["text"].as_str().unwrap_or("").to_string())
}

fn chat(client: &reqwest::blocking::Client, cfg: &Config, user: &str) -> Result<String> {
    let body = serde_json::json!({
        "model": cfg.chat_model,
        "messages": [
            {"role": "system", "content":
                "You are a friendly voice assistant on a small speaker. Reply in a concise, \
                 natural, spoken style — usually 1-2 short sentences. No markdown or emojis."},
            {"role": "user", "content": user}
        ]
    });
    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .context("chat request")?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        return Err(anyhow!("chat {status}: {text}"));
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    Ok(v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

fn synthesize(client: &reqwest::blocking::Client, cfg: &Config, text: &str) -> Result<Vec<u8>> {
    let body = serde_json::json!({
        "model": cfg.tts_model,
        "voice": cfg.tts_voice,
        "input": text,
        "response_format": "pcm"
    });
    let resp = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .context("tts request")?;
    let status = resp.status();
    if !status.is_success() {
        let t = resp.text().unwrap_or_default();
        return Err(anyhow!("tts {status}: {t}"));
    }
    Ok(resp.bytes()?.to_vec())
}

// ─────────────────────────── audio helpers ───────────────────────────

/// Wrap s16le mono PCM in a 44-byte WAV header.
fn pcm_to_wav(pcm: &[u8], rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = rate * 2;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// Extract the PCM `data` chunk from a WAV byte buffer.
fn wav_to_pcm(wav: &[u8]) -> Vec<u8> {
    if let Some(pos) = wav.windows(4).position(|w| w == b"data") {
        let start = pos + 8;
        if start <= wav.len() {
            return wav[start..].to_vec();
        }
    }
    Vec::new()
}

/// Linear-resample s16le mono PCM from `from` Hz to `to` Hz.
fn resample(pcm: &[u8], from: u32, to: u32) -> Vec<u8> {
    if from == to {
        return pcm.to_vec();
    }
    let src: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    if src.is_empty() {
        return Vec::new();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((src.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len * 2);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let a = src[idx] as f64;
        let b = *src.get(idx + 1).unwrap_or(&src[idx]) as f64;
        let s = (a + (b - a) * frac).round() as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn log(msg: &str) {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    println!("[{:7.2}s] {msg}", start.elapsed().as_secs_f32());
}
