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

/// A PC command the agent proposed, awaiting the user's spoken yes/no.
static PENDING_CMD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// What to send back to the device.
struct Response {
    /// New speaker volume 0..=100, or None to leave it unchanged.
    volume: Option<u8>,
    /// Response audio (16 kHz mono s16le PCM).
    pcm: Vec<u8>,
}

/// Recognize a spoken volume command like "set volume 60" / "volume to eighty".
/// Returns the requested level 0..=100 if the transcript is a volume command.
fn parse_volume(transcript: &str) -> Option<u8> {
    let t = transcript.to_lowercase();
    if !t.contains("volume") && !t.contains("vol ") {
        return None;
    }
    // Digits first (e.g. "set volume 60").
    let mut num = String::new();
    for ch in t.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if !num.is_empty() {
            break;
        }
    }
    if let Ok(n) = num.parse::<u32>() {
        return Some(n.min(100) as u8);
    }
    // Spoken number words (volume is usually round).
    for (word, val) in [
        ("hundred", 100u8), ("ninety", 90), ("eighty", 80), ("seventy", 70),
        ("sixty", 60), ("fifty", 50), ("forty", 40), ("thirty", 30),
        ("twenty", 20), ("ten", 10), ("zero", 0), ("mute", 0),
        ("max", 100), ("full", 100), ("min", 0),
    ] {
        if t.contains(word) {
            return Some(val);
        }
    }
    None
}

struct Config {
    api_key: String,
    port: u16,
    chat_model: String,
    tts_model: String,
    tts_voice: String,
    stt_model: String,
    mode: String,
    stt_engine: String, // "sapi" (Windows System.Speech) | "whisper" (local HTTP)
    stt_url: String,
    stt_script: PathBuf,
    tts_script: PathBuf,
    tts_speed: String,
    agent: bool, // allow voice commands to run PC actions (with spoken confirmation)
    debug_wav: bool,
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
        stt_engine: env_or("STT_ENGINE", "whisper").to_lowercase(),
        stt_url: env_or("STT_URL", "http://127.0.0.1:9100/stt"),
        stt_script,
        tts_script,
        tts_speed: env_or("TTS_SPEED", "1.3"), // 1.0 = normal, higher = faster
        agent: env_or("AGENT", "1") != "0",
        debug_wav: std::env::var("DEBUG_WAV").is_ok(),
    };

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).with_context(|| format!("bind {addr}"))?;
    log(&format!("MODE = {}", cfg.mode));
    match cfg.mode.as_str() {
        "windows" => {
            let stt = if cfg.stt_engine == "whisper" {
                format!("local Whisper @ {}", cfg.stt_url)
            } else {
                "Windows System.Speech".to_string()
            };
            log(&format!("STT: {stt}  |  reply: claude CLI  |  TTS: Windows SAPI"));
        }
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

    if cfg.debug_wav {
        let _ = std::fs::write("debug_last.wav", pcm_to_wav(&pcm, DEVICE_RATE));
        let _ = std::fs::write("debug_last.pcm", &pcm);
        log("[debug] saved debug_last.wav / debug_last.pcm");
    }

    // 2. Produce the response according to the mode.
    let resp = match cfg.mode.as_str() {
        "loopback" => {
            log(&format!("[loopback] echoing {} bytes", pcm.len()));
            Response {
                volume: None,
                pcm: pcm.clone(),
            }
        }
        "windows" => windows_brain(&pcm, cfg)?,
        "openai" => openai_brain(&pcm, cfg)?,
        other => {
            log(&format!("[error] unknown MODE '{other}'"));
            return Ok(());
        }
    };

    if resp.pcm.is_empty() && resp.volume.is_none() {
        log("[skip] nothing to send");
        return Ok(());
    }

    // 3. Send: 1 control byte (0xFF = no volume change, else 0..=100) + PCM.
    let ctrl = resp.volume.unwrap_or(0xFF);
    if let Some(v) = resp.volume {
        log(&format!("[volume] -> {v}"));
    }
    stream.write_all(&[ctrl]).context("writing control header")?;
    if !resp.pcm.is_empty() {
        stream.write_all(&resp.pcm).context("writing response PCM")?;
    }
    stream.flush().ok();
    log(&format!("[done] total {:?}", t0.elapsed()));
    Ok(())
}

// ───────────────────────── Windows-native brain ─────────────────────────

fn windows_brain(pcm: &[u8], cfg: &Config) -> Result<Response> {
    // STT (Windows System.Speech).
    let t = Instant::now();
    let transcript = windows_stt(cfg, pcm)?;
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript));
    if transcript.is_empty() {
        log("[skip] empty transcript (mic too quiet or not recognized)");
        return Ok(Response { volume: None, pcm: Vec::new() });
    }

    // Agent mode: is this a "yes/no" answering a previously proposed action?
    if cfg.agent {
        let pending = PENDING_CMD.lock().unwrap().take();
        if let Some(cmd) = pending {
            let ru = is_cyrillic(&transcript);
            if is_affirmative(&transcript) {
                let ok = run_pc_command(&cmd);
                log(&format!("[agent] run `{cmd}` -> {}", if ok { "ok" } else { "FAILED" }));
                let say = match (ru, ok) {
                    (true, true) => "Готово.",
                    (true, false) => "Не получилось.",
                    (false, true) => "Done.",
                    (false, false) => "That didn't work.",
                };
                return Ok(Response { volume: None, pcm: windows_tts(cfg, say)? });
            } else if is_negative(&transcript) {
                log("[agent] user declined the action");
                let say = if ru { "Хорошо, отменяю." } else { "Okay, cancelled." };
                return Ok(Response { volume: None, pcm: windows_tts(cfg, say)? });
            }
            // Neither yes nor no: drop the pending action and treat as a new request.
            log("[agent] ambiguous reply, discarding pending action");
        }
    }

    // Voice command: "set volume N" — apply on the device, confirm by speech.
    if let Some(v) = parse_volume(&transcript) {
        let pcm = windows_tts(cfg, &format!("Volume set to {v}."))?;
        return Ok(Response { volume: Some(v), pcm });
    }

    // Ask claude either to ANSWER or to PROPOSE a PC command (agent mode), or
    // just to answer (agent off).
    let t = Instant::now();
    let prompt = if cfg.agent { agent_prompt(&transcript) } else { voice_prompt(&transcript) };
    let raw = run_claude(&prompt)?;
    log(&format!("[llm {:?}] {}", t.elapsed(), raw.replace('\n', " ").trim()));

    if cfg.agent {
        if let Some((true, say, Some(cmd))) = parse_decision(&raw) {
            *PENDING_CMD.lock().unwrap() = Some(cmd.clone());
            log(&format!("[agent] proposed command: {cmd}"));
            let ask = if is_cyrillic(&say) { " Сказать да или нет?" } else { " Say yes or no." };
            let speak = format!("{}{}", clean_for_speech(&say), ask);
            return Ok(Response { volume: None, pcm: windows_tts(cfg, &speak)? });
        }
        // Not an action: speak the answer (from JSON `say`, else the raw text).
        let say = parse_decision(&raw)
            .map(|(_, s, _)| s)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| raw.clone());
        return Ok(Response { volume: None, pcm: windows_tts(cfg, &clean_for_speech(&say))? });
    }

    let reply = clean_for_speech(&raw);
    if reply.is_empty() {
        return Ok(Response { volume: None, pcm: Vec::new() });
    }
    let pcm = windows_tts(cfg, &reply)?;
    Ok(Response { volume: None, pcm })
}

/// True if the text contains any Cyrillic letters (used to pick the reply language).
fn is_cyrillic(t: &str) -> bool {
    t.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

fn has_word(t: &str, words: &[&str]) -> bool {
    let lt = t.to_lowercase();
    lt.split(|c: char| !c.is_alphanumeric())
        .any(|w| words.contains(&w))
}

fn is_affirmative(t: &str) -> bool {
    has_word(
        t,
        &["yes", "yeah", "yep", "yup", "sure", "ok", "okay", "да", "ага", "давай", "конечно", "угу", "ладно"],
    )
}

fn is_negative(t: &str) -> bool {
    has_word(
        t,
        &["no", "nope", "cancel", "stop", "нет", "отмена", "отмени", "стоп", "неа"],
    )
}

/// Run a Windows command (the agent's proposed action) detached. Returns spawn success.
fn run_pc_command(cmd: &str) -> bool {
    Command::new("cmd")
        .args(["/C", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Parse the agent decision JSON -> (action, say, cmd).
fn parse_decision(text: &str) -> Option<(bool, String, Option<String>)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let action = v["action"].as_bool().unwrap_or(false);
    let say = v["say"].as_str().unwrap_or("").to_string();
    let cmd = v["cmd"].as_str().map(str::to_string).filter(|s| !s.is_empty());
    Some((action, say, cmd))
}

/// Prompt that lets claude either answer or propose a PC command (as JSON).
fn agent_prompt(transcript: &str) -> String {
    let now_local = chrono::Local::now().format("%A %Y-%m-%d %H:%M %:z");
    format!(
        "You are a voice assistant that can ANSWER questions or CONTROL this Windows PC. \
         Current local time: {now_local} (use it for time questions; convert zones from UTC). \
         The user said (transcribed speech, may contain errors): \"{transcript}\".\n\
         If the user wants to DO something on the PC (open a folder/app/file, launch a \
         program, run a command, create/move files), reply with EXACTLY this JSON:\n\
         {{\"action\":true,\"say\":\"<one short sentence in the USER'S language describing what you will do>\",\"cmd\":\"<a single Windows cmd.exe command that does it; use %USERPROFILE% for the home folder>\"}}\n\
         Otherwise (a question or chat; you MAY use web search for live facts), reply with:\n\
         {{\"action\":false,\"say\":\"<spoken answer in the USER'S language, 1-2 short sentences, no URLs or markdown>\"}}\n\
         Output ONLY the JSON object and nothing else."
    )
}

/// Transcribe device PCM. Default engine is Windows System.Speech (pure Windows,
/// no Python); set STT_ENGINE=whisper to use the local Whisper microservice.
fn windows_stt(cfg: &Config, pcm: &[u8]) -> Result<String> {
    if cfg.stt_engine == "whisper" {
        let client = reqwest::blocking::Client::new();
        let resp = client
            .post(&cfg.stt_url)
            .body(pcm.to_vec())
            .send()
            .with_context(|| format!("POST {} — is stt_server.py running?", cfg.stt_url))?;
        let body = resp.text()?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        return Ok(v["text"].as_str().unwrap_or("").trim().to_string());
    }

    // Windows System.Speech: write a WAV and run the recognizer script.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let wav_in = std::env::temp_dir().join(format!("vs3r_in_{id}.wav"));
    std::fs::write(&wav_in, pcm_to_wav(pcm, DEVICE_RATE))?;
    let text = run_ps(&cfg.stt_script, &["-Wav", path_str(&wav_in)?])?;
    cleanup(&[&wav_in]);
    Ok(text.trim().to_string())
}

/// Synthesize text to 16 kHz mono PCM with Windows SAPI.
fn windows_tts(cfg: &Config, text: &str) -> Result<Vec<u8>> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let txt = std::env::temp_dir().join(format!("vs3r_reply_{id}.txt"));
    let wav_out = std::env::temp_dir().join(format!("vs3r_out_{id}.wav"));
    std::fs::write(&txt, text)?;
    run_ps(
        &cfg.tts_script,
        &["-TextFile", path_str(&txt)?, "-Out", path_str(&wav_out)?, "-Rate", &cfg.tts_speed],
    )?;
    let wav = std::fs::read(&wav_out)?;
    cleanup(&[&txt, &wav_out]);
    Ok(wav_to_pcm(&wav))
}

/// Strip anything that shouldn't be spoken aloud: a trailing Sources/URL list,
/// markdown links/emphasis. Web search tends to append citations.
fn clean_for_speech(text: &str) -> String {
    let mut s = text.trim().to_string();
    // Drop a trailing sources/citations section.
    let lower = s.to_lowercase();
    for marker in ["\nsources:", "\nsource:", "\nreferences:", "\ncitations:"] {
        if let Some(i) = lower.find(marker) {
            s.truncate(i);
            break;
        }
    }
    // Markdown link [text](url) -> text
    while let (Some(open), Some(close)) = (s.find("]("), s.find("](").and_then(|i| s[i..].find(')').map(|j| i + j))) {
        if let Some(lb) = s[..open].rfind('[') {
            let label = s[lb + 1..open].to_string();
            s.replace_range(lb..=close, &label);
        } else {
            break;
        }
    }
    s.replace('*', "").replace('`', "").replace('#', "").trim().to_string()
}

/// Shared voice-assistant instruction wrapped around the transcript.
fn voice_prompt(transcript: &str) -> String {
    let now_local = chrono::Local::now().format("%A %Y-%m-%d %H:%M %:z");
    let now_utc = chrono::Utc::now().format("%H:%M UTC");
    format!(
        "Current time is {now_local} (= {now_utc}); this server's local time is the \
         user's local time. Use it for any time/date question and compute other time \
         zones from UTC — do NOT guess or web-search the time. \
         You are a warm, concise voice assistant speaking through a small smart speaker. \
         Answer the user's spoken words directly in 1-2 short sentences of plain speech. \
         Reply in the SAME language the user used (Russian or English). \
         You may search the web for current info (weather, news, facts) and answer with it, \
         but speak ONLY the answer: never read out sources, URLs, citations, markdown, \
         lists, or emojis. Never mention coding, tools, or that you are an AI/CLI. If the \
         words are unclear, make a friendly best guess rather than asking what they meant.\n\n\
         User said: {transcript}"
    )
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
        .args([
            "/C", "claude", "-p",
            "--tools", "WebSearch",        // make web search available
            "--allowedTools", "WebSearch", // pre-approve it (headless, no prompt)
        ])
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

/// Write the Windows System.Speech STT + SAPI TTS helper scripts; return paths.
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

// WinRT TTS: reaches OneCore voices (incl. Russian Irina/Pavel), auto-selects a
// voice by language (Cyrillic -> Russian, else English), outputs 16 kHz mono WAV.
const TTS_PS: &str = r#"param([Parameter(Mandatory=$true)][string]$TextFile,[Parameter(Mandatory=$true)][string]$Out,[double]$Rate=1.0)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$text = [System.IO.File]::ReadAllText($TextFile)
[void][Windows.Media.SpeechSynthesis.SpeechSynthesizer, Windows.Media, ContentType=WindowsRuntime]
[void][Windows.Storage.Streams.DataReader, Windows.Storage.Streams, ContentType=WindowsRuntime]
$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
  $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
  $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
function Await($op, $resultType) {
  $m = $asTask.MakeGenericMethod($resultType)
  $t = $m.Invoke($null, @($op)); $t.Wait(-1) | Out-Null; $t.Result
}
$synth = New-Object Windows.Media.SpeechSynthesis.SpeechSynthesizer
$synth.Options.SpeakingRate = [Math]::Min([Math]::Max($Rate, 0.5), 6.0)
$isRu = $text -match '\p{IsCyrillic}'
$pick = $null
foreach ($v in [Windows.Media.SpeechSynthesis.SpeechSynthesizer]::AllVoices) {
  if ($isRu) { if ($v.Language -like 'ru*') { $pick = $v; break } }
  else       { if ($v.Language -like 'en*') { $pick = $v; break } }
}
if ($pick) { $synth.Voice = $pick }
$streamType = [Windows.Media.SpeechSynthesis.SpeechSynthesisStream]
$stream = Await ($synth.SynthesizeTextToStreamAsync($text)) $streamType
$size = [uint32]$stream.Size
$reader = New-Object Windows.Storage.Streams.DataReader($stream)
[void](Await ($reader.LoadAsync($size)) ([uint32]))
$bytes = New-Object byte[] $size
$reader.ReadBytes($bytes)
[System.IO.File]::WriteAllBytes($Out, $bytes)
"#;

// ───────────────────────────── OpenAI brain ─────────────────────────────

fn openai_brain(pcm: &[u8], cfg: &Config) -> Result<Response> {
    let client = reqwest::blocking::Client::new();

    let t = Instant::now();
    let wav = pcm_to_wav(pcm, DEVICE_RATE);
    let transcript = transcribe(&client, cfg, wav)?.trim().to_string();
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript));
    if transcript.is_empty() {
        return Ok(Response { volume: None, pcm: Vec::new() });
    }

    // Voice command: "set volume N".
    if let Some(v) = parse_volume(&transcript) {
        let pcm = openai_tts(&client, cfg, &format!("Volume set to {v}."))?;
        return Ok(Response { volume: Some(v), pcm });
    }

    let t = Instant::now();
    let reply = clean_for_speech(&chat(&client, cfg, &transcript)?);
    log(&format!("[llm {:?}] \"{}\"", t.elapsed(), reply));

    let pcm = openai_tts(&client, cfg, &reply)?;
    Ok(Response { volume: None, pcm })
}

/// OpenAI TTS (24 kHz PCM) resampled to the device's 16 kHz.
fn openai_tts(client: &reqwest::blocking::Client, cfg: &Config, text: &str) -> Result<Vec<u8>> {
    let t = Instant::now();
    let tts_pcm = synthesize(client, cfg, text)?;
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
