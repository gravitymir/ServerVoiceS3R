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
//! Other env: PORT (9000), CHAT_MODEL, TTS_MODEL, TTS_VOICE_SOPHIA, TTS_VOICE_JARVIS, STT_MODEL.

use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::process::CommandExt; // creation_flags (CREATE_NO_WINDOW)
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

const DEVICE_RATE: u32 = 16_000; // device PCM sample rate
const TTS_RATE: u32 = 24_000; // OpenAI TTS pcm sample rate

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A PC command the agent proposed, awaiting the user's spoken yes/no.
static PENDING_CMD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Last volume the skills agent set on the device (so it can reason about
/// relative requests like "louder" / "тише").
static LAST_VOLUME: std::sync::Mutex<u8> = std::sync::Mutex::new(75);

/// M6 voice coding mode: while true, spoken commands are routed to a persistent
/// Claude Code session in `code_dir` instead of the normal skills.
static CODING_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether the current coding-mode session has been started (controls --continue).
static CODING_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Chat mode ("just talk"): while true, spoken utterances are routed to a
/// persistent Claude conversation (web search, but NO file/shell tools) in a
/// separate thread — the voice equivalent of the desktop app's "Chat" tab.
static CHAT_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether the current chat session has been started (controls --continue).
static CHAT_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// HACKER mode (entertainment): while true, every utterance is answered with an
/// absurd, over-the-top FICTIONAL "successful hack" report. Pure comedy — never
/// real instructions.
static HACKER_MODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Translate mode: while true, each utterance is translated (Google Translate) to
/// TRANSLATE_TARGET and spoken back. Entered by voice (brain picks the target),
/// left by a voice exit phrase.
static TRANSLATE_MODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Target language (ISO-639-1, e.g. "en", "es") for translate mode.
static TRANSLATE_TARGET: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// Continuous transcribe mode: while true, each utterance is transcribed + printed
/// (no LLM, no spoken reply). Entered by voice, left by the device's button marker
/// or by TRANSCRIBE_TIMEOUT seconds of no speech.
static TRANSCRIBE_MODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Timestamp of the last actual speech in transcribe mode (for the idle timeout).
static TRANSCRIBE_LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
/// "Live dictation": when true, each transcribed phrase is pasted (Ctrl+V) into
/// whatever Windows field currently has focus, not just printed/clipboarded.
static TYPE_INTO_FOCUS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// How pasted phrases are separated: 0 = leading space, 1 = newline (each phrase
/// on its own line), 2 = none (joined). Voice-configurable. Default = newline
/// (each phrase on a new line, no spaces added).
static TRANSCRIBE_SEP: AtomicU8 = AtomicU8::new(1);
/// Whether the current dictation session has typed its first phrase. The first
/// phrase gets NO leading separator; every phrase after it gets the separator in
/// front. Reset to false when a transcribe session starts.
static DICTATION_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Live-dictation delivery method: 0 = paste (clipboard + Ctrl+V), 1 = type
/// (native Win32 SendInput). Paste is the default — a PASTED newline is dropped
/// by single-line fields (e.g. a search box), whereas a synthesized Enter /
/// Shift+Enter can submit them. Config: DICTATION_METHOD; voice-switchable.
static DICTATION_METHOD: AtomicU8 = AtomicU8::new(0);
/// Consecutive silent turns while in a keep-listening mode (chat/translate/etc.).
/// After enough of them (~60 s) the mode auto-exits so a forgotten or unheard
/// session doesn't loop forever. Reset to 0 on any real-speech turn.
static SILENCE_STREAK: AtomicU8 = AtomicU8::new(0);

/// Device→server header bit (OR'd into the persona byte): "button pressed — leave
/// transcribe mode". The accompanying utterance is empty.
const HDR_TRANSCRIBE_EXIT: u8 = 0x80;

/// Control byte sent before the PCM: 0xFF = no change, 0..=100 = set volume,
/// 0xFE = enter speaker mode (device connects to the pc_speaker stream),
/// 0xFD = stay in continuous transcribe mode (record the next utterance with no
/// wake word). Anything else while transcribing means "leave transcribe mode".
const CTRL_NONE: u8 = 0xFF;
const CTRL_SPEAKER: u8 = 0xFE;
const CTRL_TRANSCRIBE: u8 = 0xFD;
/// Start a STREAMING transcribe session: the device opens one long-lived
/// connection to TRANSCRIBE_STREAM_PORT and pushes the mic continuously; the
/// server segments + transcribes it. LOCAL = on-PC Whisper, EXTERNAL = OpenAI.
const CTRL_STREAM_LOCAL: u8 = 0xFC;
const CTRL_STREAM_EXTERNAL: u8 = 0xFB;
/// Port the device streams the mic to for streaming transcription.
const TRANSCRIBE_STREAM_PORT: u16 = 9002;

/// Online radio. Reuses SPEAKER MODE: the `radio` skill starts an ffmpeg that
/// decodes a live stream to 16 kHz mono PCM, replies with CTRL_SPEAKER, and the
/// device connects to this port (same as `pc_speaker`) and plays it until the
/// button is pressed (button also exits + listens for the next command, so you
/// switch stations by: press button -> say the next station). Don't run
/// `pc_speaker.exe` at the same time — both want this port.
const RADIO_STREAM_PORT: u16 = 9001;
/// The running ffmpeg decoder for the current station (killed on stop/switch).
static RADIO_CHILD: std::sync::Mutex<Option<Child>> = std::sync::Mutex::new(None);
/// Its stdout (raw 16 kHz mono PCM), handed to the radio-stream connection handler.
static RADIO_STDOUT: std::sync::Mutex<Option<ChildStdout>> = std::sync::Mutex::new(None);
/// PC-audio (WASAPI loopback) captured as 16 kHz mono s16le — the folded-in
/// `pc_speaker`. Speaker mode mirrors this when no radio station is active, so
/// one process/port serves both. Bounded in the capture callback (~0.5 s).
static LOOPBACK_BUF: std::sync::Mutex<VecDeque<u8>> = std::sync::Mutex::new(VecDeque::new());

/// What to send back to the device.
struct Response {
    /// Leading control byte (see CTRL_* constants / 0..=100 volume).
    control: u8,
    /// Response audio (16 kHz mono s16le PCM).
    pcm: Vec<u8>,
}

/// Recognize a spoken volume command like "set volume 60" / "volume to eighty".
/// Returns the requested level 0..=100 if the transcript is a volume command.
fn parse_volume(transcript: &str) -> Option<u8> {
    let t = transcript.to_lowercase();
    // Trigger word in English or Russian. "мкост" tolerates common Whisper
    // mis-hearings of "громкость" (гломкость / грамкость / ...).
    let is_vol = t.contains("volume") || t.contains("vol ")
        || t.contains("мкост") || t.contains("звук") || t.contains("громк") || t.contains("гломк");
    if !is_vol {
        return None;
    }
    // First run of digits, e.g. "set volume 60" / "громкость 60".
    let digits: String = t
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if let Ok(n) = digits.parse::<u32>() {
        return Some(n.min(100) as u8);
    }
    // Spoken number words (whole-word match), English + Russian.
    let words: &[(&str, u8)] = &[
        ("hundred", 100), ("ninety", 90), ("eighty", 80), ("seventy", 70),
        ("sixty", 60), ("fifty", 50), ("forty", 40), ("thirty", 30),
        ("twenty", 20), ("ten", 10), ("zero", 0), ("mute", 0), ("max", 100), ("full", 100), ("min", 0),
        ("сто", 100), ("девяносто", 90), ("восемьдесят", 80), ("семьдесят", 70),
        ("шестьдесят", 60), ("пятьдесят", 50), ("сорок", 40), ("тридцать", 30),
        ("двадцать", 20), ("десять", 10), ("ноль", 0), ("максимум", 100), ("макс", 100),
    ];
    for (w, v) in words {
        if has_word(&t, &[w]) {
            return Some(*v);
        }
    }
    None
}

struct Config {
    api_key: String,
    port: u16,
    chat_model: String,
    tts_model: String,
    tts_voice_sophia: String,
    stt_model: String,
    mode: String,
    stt_engine: String, // "sapi" (Windows System.Speech) | "whisper" (local HTTP)
    stt_url: String,
    stt_script: PathBuf,
    tts_script: PathBuf,
    tts_speed: String,
    agent: bool, // allow voice commands to run PC actions (with spoken confirmation)
    debug_wav: bool,
    code_dir: String, // project folder for voice "coding mode" (M6)
    chat_dir: String, // separate folder for voice "chat mode" (own --continue thread)
    tts_voice_jarvis: String, // OpenAI TTS voice for the male "Jarvis" persona
    transcribe_timeout_secs: u64, // auto-leave transcribe mode after this much silence
    realtime_model: String, // OpenAI Realtime transcription model (external streaming)
    realtime_silence_ms: u64, // server-VAD silence before a segment ends (Realtime)
    google_translate_key: String, // Google Cloud Translation API key (translate mode)
    radio_favorites: Vec<RadioStation>, // online-radio favorites
    compressor_host: String, // StamPLC compressor IP/host (empty = skill disabled)
    compressor_net: String,  // local-IP prefix that enables the compressor skill (e.g. "192.168.3.")
}

/// An online-radio favorite: display name, optional pinned stream URL, and extra
/// spoken aliases (e.g. Latin spellings so an English request matches a
/// Cyrillic-named station: "Кис ФМ" ← "kiss").
struct RadioStation {
    name: String,
    url: Option<String>,
    aliases: Vec<String>,
}

/// Which on-device wake word fired (sent as the first request byte). Selects the
/// spoken voice and the persona injected into the brain/coding prompts.
const PERSONA_SOPHIA: u8 = 0;
const PERSONA_JARVIS: u8 = 1;

struct Persona {
    name: &'static str,
    voice: String, // OpenAI TTS voice for this persona
    skills_intro: &'static str,
    coding_intro: &'static str,
}

impl Persona {
    fn from_byte(b: u8, cfg: &Config) -> Persona {
        // PERSONA_SOPHIA is the default for anything that isn't explicitly Jarvis.
        let _ = PERSONA_SOPHIA;
        if b == PERSONA_JARVIS {
            Persona {
                name: "Jarvis",
                voice: cfg.tts_voice_jarvis.clone(),
                skills_intro: "You are Jarvis, a calm, capable MALE voice assistant — always speak \
                    about yourself as a man; in Russian ALWAYS use masculine grammatical forms (e.g. \
                    'сделал', 'готов', 'рад', never 'сделала'/'готова').",
                coding_intro: "You are Jarvis, a hands-free MALE voice coding assistant in this \
                    project directory (always speak about yourself as a man; in Russian ALWAYS use \
                    masculine forms like 'сделал', 'запустил', 'готов', never feminine).",
            }
        } else {
            Persona {
                name: "Sophia",
                voice: cfg.tts_voice_sophia.clone(),
                skills_intro: "You are Sophia, a warm, friendly FEMALE voice assistant — always speak \
                    about yourself as a woman; in Russian ALWAYS use feminine grammatical forms (e.g. \
                    'сделала', 'готова', 'рада', never 'сделал'/'готов').",
                coding_intro: "You are Sophia, a hands-free FEMALE voice coding assistant in this \
                    project directory (always speak about yourself as a woman; in Russian ALWAYS use \
                    feminine forms like 'сделала', 'запустила', 'готова', never masculine).",
            }
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Load config from a `.env` file (KEY=VALUE per line) next to the exe, falling
/// back to the current directory. Lines starting with `#` are comments. Values
/// already set in the real environment win (env overrides .env).
fn load_dotenv() {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(".env"));
        }
    }
    candidates.push(std::path::PathBuf::from(".env"));

    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let mut v = v.trim();
                // Strip an inline comment: a '#' that follows whitespace (so a '#'
                // inside a value, e.g. a URL fragment, without a space is kept).
                if let Some(i) = v
                    .char_indices()
                    .find_map(|(i, c)| (c == '#' && v[..i].ends_with([' ', '\t'])).then_some(i))
                {
                    v = v[..i].trim_end();
                }
                if v.len() >= 2
                    && ((v.starts_with('"') && v.ends_with('"'))
                        || (v.starts_with('\'') && v.ends_with('\'')))
                {
                    v = &v[1..v.len() - 1];
                }
                if !k.is_empty() && std::env::var(k).is_err() {
                    std::env::set_var(k, v);
                }
            }
        }
        log(&format!("loaded config from {}", path.display()));
        return;
    }
}

fn main() -> Result<()> {
    load_dotenv(); // read KEY=VALUE config from a .env file next to the exe

    let mode = if std::env::var("LOOPBACK").is_ok() {
        "loopback".to_string()
    } else {
        env_or("MODE", "windows").to_lowercase()
    };

    let api_key = if mode == "openai" || mode == "skills" {
        std::env::var("OPENAI_API_KEY")
            .map_err(|_| anyhow!("MODE={mode} needs OPENAI_API_KEY (for Whisper STT + TTS)"))?
    } else {
        String::new()
    };

    let (stt_script, tts_script) = write_scripts()?;

    let cfg = Config {
        api_key,
        port: env_or("PORT", "9000").parse().unwrap_or(9000),
        chat_model: env_or("CHAT_MODEL", "gpt-4o-mini"),
        tts_model: env_or("TTS_MODEL", "gpt-4o-mini-tts"),
        tts_voice_sophia: env_or("TTS_VOICE_SOPHIA", "nova"),
        stt_model: env_or("STT_MODEL", "whisper-1"),
        mode,
        stt_engine: env_or("STT_ENGINE", "whisper").to_lowercase(),
        stt_url: env_or("STT_URL", "http://127.0.0.1:9100/stt"),
        stt_script,
        tts_script,
        tts_speed: env_or("TTS_SPEED", "1.3"), // 1.0 = normal, higher = faster
        agent: env_or("AGENT", "1") != "0",
        debug_wav: std::env::var("DEBUG_WAV").is_ok(),
        code_dir: env_or("CODE_DIR", "C:/Users/gravi/voice-code"),
        chat_dir: {
            // Own directory so chat's `--continue` thread stays separate from coding's.
            let c = env_or("CHAT_DIR", "");
            if c.trim().is_empty() {
                std::env::temp_dir().join("s3r_chat").to_string_lossy().into_owned()
            } else {
                c
            }
        },
        tts_voice_jarvis: env_or("TTS_VOICE_JARVIS", "onyx"),
        transcribe_timeout_secs: env_or("TRANSCRIBE_TIMEOUT", "60").parse().unwrap_or(60),
        realtime_model: env_or("REALTIME_MODEL", "gpt-4o-transcribe"),
        realtime_silence_ms: env_or("REALTIME_SILENCE_MS", "1500").parse().unwrap_or(1500),
        google_translate_key: env_or("GOOGLE_TRANSLATE_API_KEY", ""),
        radio_favorites: load_radio_favorites(),
        compressor_host: env_or("COMPRESSOR_HOST", "").trim().to_string(),
        compressor_net: env_or("COMPRESSOR_NET", "192.168.3.").trim().to_string(),
    };

    // Live dictation: type transcripts into the focused field.
    // Default ON — only an explicit 0/false/no/off disables it.
    let tf = env_or("TYPE_INTO_FOCUS", "").trim().to_lowercase();
    let type_focus = !matches!(tf.as_str(), "0" | "false" | "no" | "off");
    TYPE_INTO_FOCUS.store(type_focus, Ordering::Relaxed);

    // Transcribe-mode separator between dictated phrases. Default = NEW LINE
    // (each phrase on its own line). Voice command "настройки для транскрибации"
    // can change it at runtime; this just sets the startup value from the config.
    let sep = env_or("TRANSCRIBE_SEP", "").trim().to_lowercase();
    let sep_val: u8 = match sep.as_str() {
        "space" | "пробел" | "0" => 0,
        "none" | "joined" | "слитно" | "no" | "2" => 2,
        _ => 1, // newline (default; also "newline"/"line"/"новая строка"/"1"/empty)
    };
    TRANSCRIBE_SEP.store(sep_val, Ordering::Relaxed);

    // Live-dictation delivery method. Default = paste (clipboard + Ctrl+V); a
    // pasted newline doesn't submit single-line fields. "type" uses native
    // Win32 SendInput. Voice-switchable at runtime.
    let dm = env_or("DICTATION_METHOD", "").trim().to_lowercase();
    let method_val: u8 = match dm.as_str() {
        "type" | "native" | "sendinput" | "keys" | "1" => 1,
        _ => 0, // paste / clipboard / ctrl+v / empty (default)
    };
    DICTATION_METHOD.store(method_val, Ordering::Relaxed);

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
            cfg.stt_model, cfg.chat_model, cfg.tts_model, cfg.tts_voice_sophia
        )),
        "skills" => {
            std::fs::create_dir_all(&cfg.code_dir).ok();
            std::fs::create_dir_all(&cfg.chat_dir).ok();
            log(&format!(
                "Skills agent: STT=OpenAI {} | brain=claude CLI (web search) | TTS=OpenAI {} voice={}",
                cfg.stt_model, cfg.tts_model, cfg.tts_voice_sophia
            ));
            log(&format!("Coding mode (M6) project dir: {}", cfg.code_dir));
            log(&format!("Chat mode (talk + web search) dir: {}", cfg.chat_dir));
            log(&format!("Transcribe mode idle timeout: {}s", cfg.transcribe_timeout_secs));
            if type_focus {
                let sep_name = match sep_val {
                    0 => "space",
                    2 => "none",
                    _ => "newline",
                };
                let method_name = if method_val == 1 { "type (SendInput)" } else { "paste (Ctrl+V)" };
                log(&format!(
                    "Live dictation: ON — into the focused field (method: {method_name}, separator: {sep_name})"
                ));
            }
            if !cfg.google_translate_key.is_empty() {
                log("Translate mode: ready (Google Translate)");
            }
            if !cfg.compressor_host.is_empty() {
                log(&format!(
                    "Compressor skill: {} (only when local IP starts with '{}')",
                    cfg.compressor_host, cfg.compressor_net
                ));
            }
        }
        "loopback" => log("echoing recorded audio back (no AI)"),
        other => log(&format!("WARNING: unknown MODE '{other}'")),
    }
    log(&format!("listening on {addr}"));
    log("waiting for the ATOM VoiceS3R to connect (hold its button to talk)...");

    let cfg = std::sync::Arc::new(cfg);

    // Streaming-transcribe listener (port 9002): one long-lived connection per
    // session; the device pushes the mic continuously and we segment + transcribe.
    {
        let cfg = cfg.clone();
        let addr2 = format!("0.0.0.0:{}", TRANSCRIBE_STREAM_PORT);
        std::thread::spawn(move || match TcpListener::bind(&addr2) {
            Ok(l) => {
                log(&format!("transcribe stream listening on {addr2}"));
                for s in l.incoming().flatten() {
                    let cfg = cfg.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = transcribe_stream_handler(s, cfg) {
                            log(&format!("[stream error] {e:#}"));
                        }
                    });
                }
            }
            Err(e) => log(&format!("[stream] bind {addr2} failed: {e}")),
        });
    }

    // Online-radio stream listener (port 9001, same as pc_speaker): when the
    // `radio` skill fires, the device enters speaker mode and connects here; we
    // pump the current station's ffmpeg PCM until it disconnects (button press).
    if cfg.mode == "skills" || cfg.mode == "openai" {
        if !cfg.radio_favorites.is_empty() {
            let names: Vec<&str> = cfg.radio_favorites.iter().map(|s| s.name.as_str()).collect();
            log(&format!("Online radio: {} favorite(s): {}", names.len(), names.join(", ")));
        }
        let addr3 = format!("0.0.0.0:{}", RADIO_STREAM_PORT);
        std::thread::spawn(move || match TcpListener::bind(&addr3) {
            Ok(l) => {
                log(&format!("speaker/radio stream listening on {addr3}"));
                for s in l.incoming().flatten() {
                    std::thread::spawn(move || speaker_stream_handler(s));
                }
            }
            Err(e) => log(&format!("[speaker] bind {addr3} failed (another speaker server running?): {e}")),
        });
    }

    // PC-audio loopback capture (folded-in pc_speaker), kept alive for the whole
    // run. Speaker mode mirrors it whenever no radio station is playing.
    let _loopback = if cfg.mode == "skills" || cfg.mode == "openai" {
        start_loopback_capture()
    } else {
        None
    };

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

    // 1. Receive: 1 header byte + the whole utterance PCM (until the device
    //    half-closes). Header: low 7 bits = persona (wake word), bit 0x80 =
    //    "button pressed — leave transcribe mode" (the utterance is then empty).
    let mut head = [0u8; 1];
    if stream.read_exact(&mut head).is_err() {
        log("[skip] empty connection (no header byte)");
        return Ok(());
    }
    let exit_transcribe = head[0] & HDR_TRANSCRIBE_EXIT != 0;
    let persona = Persona::from_byte(head[0] & 0x7F, cfg);

    let mut pcm = Vec::new();
    stream.read_to_end(&mut pcm).context("reading utterance PCM")?;

    // Button exit from a "keep-listening" mode (transcribe / translate / hacker):
    // clear all of them and return to wake-word listening.
    if exit_transcribe {
        TRANSCRIBE_MODE.store(false, Ordering::Relaxed);
        TRANSLATE_MODE.store(false, Ordering::Relaxed);
        HACKER_MODE.store(false, Ordering::Relaxed);
        CODING_MODE.store(false, Ordering::Relaxed);
        CHAT_MODE.store(false, Ordering::Relaxed);
        SILENCE_STREAK.store(0, Ordering::Relaxed);
        // The button leaves ANY mode and just returns to the home (wake-word)
        // state — no spoken confirmation (a mode-specific phrase like "transcribe
        // mode off" was wrong when you were actually in chat/translate/etc.).
        log("[exit] button — back to wake-word listening (silent)");
        stream.write_all(&[CTRL_NONE]).ok();
        stream.flush().ok();
        return Ok(());
    }

    // Idle timeout: if transcribe mode has gone quiet for TRANSCRIBE_TIMEOUT
    // seconds (no actual speech), leave it on the next utterance.
    if TRANSCRIBE_MODE.load(Ordering::Relaxed) {
        let idle = TRANSCRIBE_LAST
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        if idle >= cfg.transcribe_timeout_secs {
            TRANSCRIBE_MODE.store(false, Ordering::Relaxed);
            log(&format!("[transcribe] auto-exit after {idle}s of silence"));
            let pcm = transcribe_off_pcm(cfg, &persona);
            stream.write_all(&[CTRL_NONE]).ok();
            if !pcm.is_empty() {
                stream.write_all(&pcm).ok();
            }
            stream.flush().ok();
            return Ok(());
        }
    }

    let secs = pcm.len() as f32 / (DEVICE_RATE as f32 * 2.0);
    log(&format!(
        "[persona] {} (voice {}){}  |  [recv] {} bytes (~{:.1}s) in {:?}",
        persona.name,
        persona.voice,
        if TRANSCRIBE_MODE.load(Ordering::Relaxed) { " [transcribe]" } else { "" },
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
                control: CTRL_NONE,
                pcm: pcm.clone(),
            }
        }
        "windows" => windows_brain(&pcm, cfg)?,
        "openai" => openai_brain(&pcm, cfg)?,
        "skills" if TRANSCRIBE_MODE.load(Ordering::Relaxed) => transcribe_turn(&pcm, cfg)?,
        "skills" => skills_brain(&pcm, cfg, &persona)?,
        other => {
            log(&format!("[error] unknown MODE '{other}'"));
            return Ok(());
        }
    };

    if resp.pcm.is_empty() && resp.control == CTRL_NONE {
        log("[skip] nothing to send");
        return Ok(());
    }

    // 3. Send: 1 control byte (0xFF none, 0..=100 volume, 0xFE speaker,
    //    0xFD continuous transcribe) + PCM.
    match resp.control {
        CTRL_NONE => {}
        CTRL_SPEAKER => log("[control] -> enter speaker mode"),
        CTRL_TRANSCRIBE => log("[control] -> transcribe (keep listening)"),
        CTRL_STREAM_LOCAL => log("[control] -> streaming transcribe (local)"),
        CTRL_STREAM_EXTERNAL => log("[control] -> streaming transcribe (external)"),
        v => log(&format!("[control] -> volume {v}")),
    }
    stream.write_all(&[resp.control]).context("writing control header")?;
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
        return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
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
                return Ok(Response { control: CTRL_NONE, pcm: windows_tts(cfg, say)? });
            } else if is_negative(&transcript) {
                log("[agent] user declined the action");
                let say = if ru { "Хорошо, отменяю." } else { "Okay, cancelled." };
                return Ok(Response { control: CTRL_NONE, pcm: windows_tts(cfg, say)? });
            }
            // Neither yes nor no: drop the pending action and treat as a new request.
            log("[agent] ambiguous reply, discarding pending action");
        }
    }

    // Voice command: "set volume N" — apply on the device, confirm by speech.
    if let Some(v) = parse_volume(&transcript) {
        let pcm = windows_tts(cfg, &format!("Volume set to {v}."))?;
        return Ok(Response { control: v, pcm });
    }

    // Voice command: "speaker mode" — tell the device to play the PC audio stream.
    if is_speaker_cmd(&transcript) {
        let say = if is_cyrillic(&transcript) {
            "Включаю режим колонки."
        } else {
            "Speaker mode on."
        };
        return Ok(Response { control: CTRL_SPEAKER, pcm: windows_tts(cfg, say)? });
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
            return Ok(Response { control: CTRL_NONE, pcm: windows_tts(cfg, &speak)? });
        }
        // Not an action: speak the answer (from JSON `say`, else the raw text).
        let say = parse_decision(&raw)
            .map(|(_, s, _)| s)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| raw.clone());
        return Ok(Response { control: CTRL_NONE, pcm: windows_tts(cfg, &clean_for_speech(&say))? });
    }

    let reply = clean_for_speech(&raw);
    if reply.is_empty() {
        return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
    }
    let pcm = windows_tts(cfg, &reply)?;
    Ok(Response { control: CTRL_NONE, pcm })
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

/// True if the user asked to enter "speaker mode" (RU/EN).
fn is_speaker_cmd(t: &str) -> bool {
    let t = t.to_lowercase();
    t.contains("speaker mode")
        || t.contains("pc speaker")
        || t.contains("колонк")
        || t.contains("динамик")
        || t.contains("режим колон")
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
            .timeout(std::time::Duration::from_secs(120))
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

// ───────────────────────── Transcribe mode ─────────────────────────────────
//
// A pure voice-to-text mode: say "transcribe mode" and from then on every
// utterance is just transcribed and PRINTED in the terminal (and copied to the
// Windows clipboard so you can paste it into any other app/website). Nothing is
// sent to the LLM and nothing is spoken back. Say "exit transcribe mode" to leave.

/// True if the text is about transcription/stenography/dictation (any language/
/// form). Broad `транскри` stem because Whisper writes "транскрипции".
fn transcribe_topic(t: &str) -> bool {
    t.contains("transcrib")
        || t.contains("voice to text")
        || t.contains("dictation")
        || t.contains("транскри")
        || t.contains("стеногра")
        || t.contains("диктов")
        || t.contains("голос в текст")
}

/// True if the text mentions pasting into the focused field (RU/EN variants).
fn mentions_field(t: &str) -> bool {
    t.contains("в поле")
        || t.contains("в input")
        || t.contains("в инпут")
        || t.contains("into field")
        || t.contains("into input")
        || t.contains("typing into")
        || t.contains("live dictation")
}
/// Voice toggle for live dictation (paste into the focused field). Check OFF first.
fn is_type_focus_off(t: &str) -> bool {
    let t = t.to_lowercase();
    mentions_field(&t)
        && (t.contains("выключ") || t.contains("отключ") || t.contains("останов")
            || t.contains("стоп") || t.contains("не ") || t.contains("off")
            || t.contains("disable") || t.contains("stop"))
}
fn is_type_focus_on(t: &str) -> bool {
    mentions_field(&t.to_lowercase())
}

/// Voice "transcribe settings": separator between pasted phrases + paste-into-field
/// toggle. Returns a spoken confirmation Response if a setting was recognized.
/// Recognizes e.g. "настройки для транскрибации: с новой строки / пробел впереди /
/// печать в поле". Must run BEFORE the transcribe-enter matchers (which also match
/// "стенограмма"/"транскрибация").
fn transcribe_settings_step(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    transcript: &str,
) -> Result<Option<Response>> {
    let t = transcript.to_lowercase();
    let speak = |words: &str| -> Result<Option<Response>> {
        Ok(Some(Response { control: CTRL_NONE, pcm: openai_tts(client, cfg, &persona.voice, words)? }))
    };

    // Separator only makes sense as a transcribe/settings instruction — require
    // that context so we don't grab unrelated speech containing "пробел"/"enter".
    let ctx = t.contains("настройк") || t.contains("транскри") || t.contains("стеногра")
        || t.contains("диктов");
    if ctx {
        if t.contains("новой строк") || t.contains("новая строк") || t.contains("новой строки")
            || t.contains("перенос") || t.contains("энтер") || t.contains("enter")
            || t.contains("new line") || t.contains("newline")
        {
            TRANSCRIBE_SEP.store(1, Ordering::Relaxed);
            log("[type] separator = newline");
            return speak("Готово, каждая фраза с новой строки.");
        }
        if t.contains("пробел") || t.contains("space") {
            TRANSCRIBE_SEP.store(0, Ordering::Relaxed);
            log("[type] separator = space");
            return speak("Готово, фразы через пробел.");
        }
        if t.contains("слитно") || t.contains("без пробел") || t.contains("без разделит") {
            TRANSCRIBE_SEP.store(2, Ordering::Relaxed);
            log("[type] separator = none");
            return speak("Готово, без разделителя.");
        }
        // Delivery method: paste (Ctrl+V) vs native key typing.
        if t.contains("ctrl") || t.contains("контрол") || t.contains("вставк")
            || t.contains("буфер") || t.contains("paste")
        {
            DICTATION_METHOD.store(0, Ordering::Relaxed);
            log("[type] method = paste (Ctrl+V)");
            return speak("Готово, вставка через Ctrl+V.");
        }
        if t.contains("нативн") || t.contains("клавиш") || t.contains("native")
            || t.contains("sendinput") || t.contains("печать клавиш")
        {
            DICTATION_METHOD.store(1, Ordering::Relaxed);
            log("[type] method = type (SendInput)");
            return speak("Готово, набор клавишами.");
        }
    }

    // Paste-into-field on/off.
    if is_type_focus_off(transcript) {
        TYPE_INTO_FOCUS.store(false, Ordering::Relaxed);
        log("[type] live dictation OFF (voice)");
        return speak("Печать в поле выключена.");
    }
    if is_type_focus_on(transcript) {
        TYPE_INTO_FOCUS.store(true, Ordering::Relaxed);
        log("[type] live dictation ON (voice)");
        return speak("Печать в поле включена.");
    }
    Ok(None)
}

/// Streaming transcribe with EXTERNAL/cloud processing ("внешняя транскрибация").
fn is_external_transcribe(t: &str) -> bool {
    let t = t.to_lowercase();
    transcribe_topic(&t) && (t.contains("внешн") || t.contains("external"))
}

fn is_transcribe_enter(t: &str) -> bool {
    transcribe_topic(&t.to_lowercase())
}

/// One continuous-transcribe utterance. Transcribe it, print it, copy it to the
/// clipboard — no LLM, no spoken reply. Non-empty speech refreshes the idle
/// timer. Returns CTRL_TRANSCRIBE so the device keeps recording; the mode is left
/// only by the device button (header bit) or the idle timeout (both in `handle`).
fn transcribe_turn(pcm: &[u8], cfg: &Config) -> Result<Response> {
    let client = reqwest::blocking::Client::new();
    let t = Instant::now();
    let wav = pcm_to_wav(pcm, DEVICE_RATE);
    let transcript = transcribe(&client, cfg, wav)?.trim().to_string();
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript));
    if !transcript.is_empty() {
        *TRANSCRIBE_LAST.lock().unwrap() = Some(Instant::now()); // refresh idle timer
        log("");
        log(&format!("📝 TRANSCRIPT ─────────────────────────────\n{transcript}\n────────────────────────────────────────────"));
        let copied = copy_to_clipboard(&transcript);
        log(&format!("[transcribe] {} chars{}", transcript.chars().count(),
            if copied { " — copied to clipboard" } else { " (clipboard copy failed)" }));
    }
    // Silent reply; CTRL_TRANSCRIBE = device records the next utterance.
    Ok(Response { control: CTRL_TRANSCRIBE, pcm: Vec::new() })
}

/// The spoken "leaving transcribe mode" confirmation (skills mode only; empty
/// otherwise). Played by the device on a button exit or an idle timeout.
fn transcribe_off_pcm(cfg: &Config, persona: &Persona) -> Vec<u8> {
    if cfg.mode != "skills" {
        return Vec::new();
    }
    let client = reqwest::blocking::Client::new();
    openai_tts(&client, cfg, &persona.voice, "Transcribe mode off.").unwrap_or_default()
}

/// Copy text to the Windows clipboard as UTF-8 (handles Cyrillic correctly, which
/// piping through `clip.exe` does not). Writes a temp file and lets PowerShell
/// read it back as UTF-8 into Set-Clipboard. Best-effort; returns success.
fn copy_to_clipboard(text: &str) -> bool {
    let tmp = std::env::temp_dir().join("s3r_transcript.txt");
    if std::fs::write(&tmp, text).is_err() {
        return false;
    }
    let cmd = format!(
        "Set-Clipboard -Value (Get-Content -Raw -Encoding UTF8 -LiteralPath '{}')",
        tmp.display()
    );
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &cmd])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Print one finished transcript segment: ONLY the spoken text, on its own line
/// (each segment follows a pause, so this gives one line per pause). No
/// timestamps or framing. Junk/empty segments are dropped. Also copies to clipboard.
fn print_transcript(text: &str) {
    let text = text.trim();
    if !is_meaningful_transcript(text) {
        return;
    }
    println!("{text}");
    let _ = std::io::stdout().flush();
    deliver_transcript(text);
}

/// Put the transcript on the clipboard, and if `TYPE_INTO_FOCUS` is on, paste it
/// (Ctrl+V) into whatever Windows field has focus — with a trailing space so
/// consecutive dictated phrases don't run together.
fn deliver_transcript(text: &str) {
    let sep = TRANSCRIBE_SEP.load(Ordering::Relaxed);
    let mut text = text.trim().to_string();

    // Voice "enter" command: a phrase ending with "нажать энтер" / "press enter"
    // (Whisper hears the two-word phrase far more reliably than a lone "enter")
    // means "type the rest, then press a real Enter" — submit a search / send a
    // message by voice. Both command words are stripped. A lone trailing
    // "enter"/"энтер" still works as a fallback (when it IS heard).
    let press_enter = {
        let stem = text.trim_end_matches(|c: char| !c.is_alphanumeric());
        let words: Vec<&str> = stem.split_whitespace().collect();
        let n = words.len();
        let is_enter = |w: &str| {
            let w = w.to_lowercase();
            w == "enter" || w == "энтер" || w == "ентер"
        };
        // Verb before "enter" (may appear without "enter" if Whisper drops it).
        let is_verb = |w: &str| {
            matches!(w.to_lowercase().as_str(), "нажать" | "нажми" | "назад" | "press")
        };
        // Verb that's safe to treat as the command ON ITS OWN when "enter" was
        // dropped. NOT "назад" — alone it just means "back".
        let is_verb_solo = |w: &str| {
            matches!(w.to_lowercase().as_str(), "нажать" | "нажми" | "press")
        };
        if n >= 2 && is_enter(words[n - 1]) && is_verb(words[n - 2]) {
            text = words[..n - 2].join(" "); // "… нажать энтер" / "… press enter"
            true
        } else if n >= 1 && is_enter(words[n - 1]) {
            text = words[..n - 1].join(" "); // "enter"/"энтер" alone (нажать dropped)
            true
        } else if n >= 1 && is_verb_solo(words[n - 1]) {
            text = words[..n - 1].join(" "); // "нажать"/"press" alone (enter dropped)
            true
        } else {
            false
        }
    };

    // In newline mode each phrase is its own line, so the period Whisper appends to
    // declarative sentences is just noise — drop a trailing '.' (and ellipsis). Keep
    // '?' and '!' (they carry meaning). In space/joined modes it reads as running
    // prose, so the period stays.
    if sep == 1 {
        while text.ends_with('.') {
            text.pop();
        }
        text = text.trim_end().to_string();
    }
    if text.is_empty() && !press_enter {
        return;
    }
    if TYPE_INTO_FOCUS.load(Ordering::Relaxed) {
        if press_enter {
            // Submit. TYPE the text (not clipboard+Ctrl+V) then press Enter, both via
            // SendInput — the OS keeps input events in call order, so Enter ALWAYS
            // lands after the typed text. (Ctrl+V is async: the Enter could fire
            // before the paste committed, which is why it "didn't work the first
            // time".) No leading separator — a submit is a fresh, discrete entry.
            if !text.is_empty() {
                type_into_focus(&text);
                let _ = copy_to_clipboard(&text); // clipboard backup
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            send_enter();
            DICTATION_STARTED.store(false, Ordering::Relaxed); // next phrase starts fresh
            log("[type] voice 'enter' — typed + Enter");
        } else if !text.is_empty() {
            // Normal dictation: deliver by the configured method, with the separator.
            // First phrase of the session: no leading separator; later ones get it.
            let first = !DICTATION_STARTED.swap(true, Ordering::Relaxed);
            let prefix = if first {
                ""
            } else {
                match sep {
                    1 => "\n", // new line: a literal '\n' for paste, Shift+Enter for type
                    2 => "",   // joined, no separator
                    _ => " ",  // space
                }
            };
            let payload = format!("{prefix}{text}");
            if DICTATION_METHOD.load(Ordering::Relaxed) == 1 {
                // Native key injection (Win32 SendInput).
                type_into_focus(&payload);
                let _ = copy_to_clipboard(&text); // keep clipboard for a manual paste
            } else {
                // Clipboard + Ctrl+V (default). A pasted newline doesn't submit
                // single-line fields, so this is the safest for search boxes.
                if !paste_into_focus(&payload) {
                    log("[type] paste failed — text left on clipboard, paste manually");
                }
            }
        }
    } else if !text.is_empty() {
        copy_to_clipboard(&text);
    }
}

/// Press Enter via native Win32 `SendInput` (VK_RETURN) — submits the focused
/// field (search box, chat send, form). Layout-independent (virtual-key code).
fn send_enter() {
    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const VK_RETURN: u16 = 0x0D;
    let key = |up: bool| KeyInput {
        type_: INPUT_KEYBOARD,
        _align: 0,
        w_vk: VK_RETURN,
        w_scan: 0,
        dw_flags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dw_extra_info: 0,
        _pad: 0,
    };
    let inputs = [key(false), key(true)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<KeyInput>() as i32,
        );
    }
}

/// Put `text` on the clipboard (UTF-8, Cyrillic-safe) and paste it with a NATIVE
/// Ctrl+V. We must NOT use PowerShell `SendKeys('^v')`: SendKeys resolves '^v'
/// through the ACTIVE keyboard layout, and on a non-US layout (e.g. Russian) there
/// is no 'v' key, so the paste silently fails. Injecting the virtual keys
/// VK_CONTROL + VK_V via SendInput is layout-independent. A pasted newline is
/// dropped by single-line fields (search boxes) instead of submitting them.
fn paste_into_focus(text: &str) -> bool {
    if !copy_to_clipboard(text) {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(30)); // let the clipboard settle
    send_ctrl_v();
    true
}

/// Press Ctrl+V via native Win32 `SendInput` using virtual-key codes — works on
/// any keyboard layout (unlike PowerShell SendKeys '^v', which fails on RU/non-US).
fn send_ctrl_v() {
    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const VK_CONTROL: u16 = 0x11;
    const VK_V: u16 = 0x56;
    let key = |vk: u16, up: bool| KeyInput {
        type_: INPUT_KEYBOARD,
        _align: 0,
        w_vk: vk,
        w_scan: 0,
        dw_flags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dw_extra_info: 0,
        _pad: 0,
    };
    let inputs = [
        key(VK_CONTROL, false), // Ctrl down
        key(VK_V, false),       // V down
        key(VK_V, true),        // V up
        key(VK_CONTROL, true),  // Ctrl up
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<KeyInput>() as i32,
        );
    }
}

/// One synthesized keyboard event (Win32 `INPUT`, keyboard variant; 40 bytes on x64).
#[repr(C)]
struct KeyInput {
    type_: u32,
    _align: u32,
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
    _pad: u64,
}

#[link(name = "user32")]
extern "system" {
    fn SendInput(c_inputs: u32, p_inputs: *const KeyInput, cb_size: i32) -> u32;
}

/// Type `text` directly into the focused field via Win32 `SendInput` (Unicode) —
/// no clipboard, no subprocess, no focus stealing. Types into whatever window the
/// user has focused, exactly like real dictation software; handles Cyrillic.
fn type_into_focus(text: &str) {
    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;
    const VK_RETURN: u16 = 0x0D;
    const VK_SHIFT: u16 = 0x10;
    // A character typed via its Unicode code unit (any letter, incl. Cyrillic).
    let unicode = |scan: u16, up: bool| KeyInput {
        type_: INPUT_KEYBOARD,
        _align: 0,
        w_vk: 0,
        w_scan: scan,
        dw_flags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dw_extra_info: 0,
        _pad: 0,
    };
    // A virtual-key press. Used for the newline separator as SHIFT+ENTER (a "soft"
    // line break): in chat-style fields (messengers, search) plain Enter would
    // SUBMIT, whereas Shift+Enter inserts a new line without sending; in plain text
    // editors it's just a normal new line. Typing the Unicode \n char alone is
    // ignored by most fields, so a real key press is required.
    let vkey = |vk: u16, up: bool| KeyInput {
        type_: INPUT_KEYBOARD,
        _align: 0,
        w_vk: vk,
        w_scan: 0,
        dw_flags: if up { KEYEVENTF_KEYUP } else { 0 },
        time: 0,
        dw_extra_info: 0,
        _pad: 0,
    };
    let mut inputs: Vec<KeyInput> = Vec::new();
    for u in text.encode_utf16() {
        if u == 0x000A || u == 0x000D {
            // Shift down, Enter down, Enter up, Shift up.
            inputs.push(vkey(VK_SHIFT, false));
            inputs.push(vkey(VK_RETURN, false));
            inputs.push(vkey(VK_RETURN, true));
            inputs.push(vkey(VK_SHIFT, true));
        } else {
            inputs.push(unicode(u, false)); // key down
            inputs.push(unicode(u, true)); // key up
        }
    }
    if inputs.is_empty() {
        return;
    }
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<KeyInput>() as i32,
        );
    }
}

/// Filter out empty / punctuation-only segments and Whisper's well-known
/// silence hallucinations (it emits "." or stock phrases on near-silence).
/// True if the text contains characters from a script we don't expect (CJK,
/// Hangul, Thai, Arabic, Hebrew, Devanagari…). Used to drop Whisper's foreign-
/// language hallucinations on noise — we only ever dictate Russian or English.
fn has_foreign_script(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x0590..=0x05FF | // Hebrew
            0x0600..=0x06FF | // Arabic
            0x0900..=0x097F | // Devanagari
            0x0E00..=0x0E7F | // Thai
            0x1100..=0x11FF | // Hangul Jamo
            0x3040..=0x30FF | // Hiragana + Katakana
            0x3400..=0x4DBF | // CJK Ext A
            0x4E00..=0x9FFF | // CJK Unified
            0xAC00..=0xD7AF | // Hangul syllables
            0xFF00..=0xFFEF)  // halfwidth/fullwidth forms
    })
}

fn is_meaningful_transcript(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || t.chars().all(|c| !c.is_alphanumeric()) {
        return false;
    }
    if has_foreign_script(t) {
        return false;
    }
    // Fragments (not whole phrases) so variants match too, e.g. "for watching"
    // catches "thanks for watching" / "thank you so much for watching".
    const JUNK: &[&str] = &[
        "продолжение следует",
        "субтитры",          // "субтитры сделал/создавал", "редактор субтитров"
        "спасибо за просмотр",
        "for watching",      // thank(s) [so much] for watching
        "subscribe",
        "subtitles",
    ];
    let low = t.to_lowercase();
    !JUNK.iter().any(|j| low.contains(j))
}

/// True if the 16-bit PCM holds enough speech-level energy to be worth
/// transcribing. On near-silence Whisper hallucinates text (foreign phrases,
/// "thanks for watching"), which in a keep-listening mode makes the assistant
/// reply to nothing — so we skip transcription entirely when it's basically quiet.
fn pcm_has_speech(pcm: &[u8]) -> bool {
    const SPEECH_PEAK: i32 = 350; // same quiet floor the firmware uses
    const MIN_LOUD_SAMPLES: usize = 1500; // ~0.1s of speech-level audio at 16 kHz
    let loud = pcm
        .chunks_exact(2)
        .filter(|s| (i16::from_le_bytes([s[0], s[1]]) as i32).abs() > SPEECH_PEAK)
        .count();
    loud >= MIN_LOUD_SAMPLES
}

// ───────────────────── Streaming transcribe (port 9002) ─────────────────────
//
// The device opens ONE long-lived connection and pushes the mic continuously.
// We segment the stream by energy (server-side VAD) and transcribe each segment
// off the read path (a worker thread), so reading never stalls and no audio is
// lost between sentences. The session ends when the device closes the stream
// (button press).

fn transcribe_stream_handler(mut stream: TcpStream, cfg: std::sync::Arc<Config>) -> Result<()> {
    let mut head = [0u8; 1];
    if stream.read_exact(&mut head).is_err() {
        return Ok(());
    }
    let persona = Persona::from_byte(head[0] & 0x7F, &cfg);
    // New session: the first dictated phrase gets no leading separator.
    DICTATION_STARTED.store(false, Ordering::Relaxed);
    log(&format!("[stream] transcribe session start (persona {})", persona.name));
    // OpenAI Realtime (word-by-word). If the websocket can't be established —
    // bad/expired/missing API key, no access, network down — fall back to
    // per-segment OpenAI Whisper REST so the session still produces text.
    match realtime_connect(&cfg) {
        Ok(ws) => stream_realtime_pump(stream, ws, debug_realtime())?,
        Err(e) => {
            log("[stream] ⚠ Realtime unavailable (check OpenAI key / network) — \
                 falling back to per-segment Whisper.");
            log(&format!("[stream]   reason: {e:#}"));
            stream_segment_loop(stream, cfg)?;
        }
    }
    log("[stream] transcribe session ended");
    Ok(())
}

/// Read the continuous PCM stream, split into speech segments by energy, and hand
/// each segment to a worker thread for STT + printing (so the read loop never
/// blocks on the network STT call → no dropped audio).
fn stream_segment_loop(mut stream: TcpStream, cfg: std::sync::Arc<Config>) -> Result<()> {
    const SPEECH_PEAK: i32 = 350; // quiet floor ~40, speech ~1000+
    const SILENCE_END_BYTES: usize = 25_600; // ~0.8s of trailing silence ends a segment (16k*2)
    const MAX_SEG_BYTES: usize = 960_000; // 30s hard cap
    const MIN_SEG_BYTES: usize = 8_000; // ~0.25s — skip clicks/blips

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let cfgw = cfg.clone();
    let worker = std::thread::spawn(move || {
        let client = reqwest::blocking::Client::new();
        for seg in rx {
            let text = transcribe(&client, &cfgw, pcm_to_wav(&seg, DEVICE_RATE)).unwrap_or_default();
            print_transcript(&text); // prints ONLY the spoken text, one line per segment
        }
    });

    let mut buf = [0u8; 2048];
    let mut acc: Vec<u8> = Vec::new(); // current segment
    let mut prev: Vec<u8> = Vec::new(); // last chunk, prepended on speech onset
    let mut spoke = false;
    let mut silence_bytes = 0usize;
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => break, // device closed (button) or error
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        let mut peak = 0i32;
        for s in chunk.chunks_exact(2) {
            let v = (i16::from_le_bytes([s[0], s[1]]) as i32).abs();
            if v > peak {
                peak = v;
            }
        }
        if peak > SPEECH_PEAK {
            if !spoke {
                acc.extend_from_slice(&prev); // pre-roll so the word onset isn't clipped
            }
            spoke = true;
            silence_bytes = 0;
            acc.extend_from_slice(chunk);
        } else if spoke {
            acc.extend_from_slice(chunk);
            silence_bytes += n;
            if silence_bytes >= SILENCE_END_BYTES || acc.len() >= MAX_SEG_BYTES {
                if acc.len() >= MIN_SEG_BYTES {
                    tx.send(std::mem::take(&mut acc)).ok();
                } else {
                    acc.clear();
                }
                spoke = false;
                silence_bytes = 0;
            }
        }
        prev.clear();
        prev.extend_from_slice(chunk);
    }
    if spoke && acc.len() >= MIN_SEG_BYTES {
        tx.send(acc).ok(); // flush trailing segment
    }
    drop(tx);
    let _ = worker.join();
    Ok(())
}

type RealtimeWs = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn debug_realtime() -> bool {
    // On only for a truthy value — `REALTIME_DEBUG=0` (or false/no/off/empty,
    // or unset) means OFF, so the terminal shows only the dictated text.
    match std::env::var("REALTIME_DEBUG") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

/// Open the OpenAI Realtime transcription websocket and configure the session.
/// Returns the connected socket (does NOT touch the device stream, so the caller
/// can cleanly fall back to per-segment STT if this fails).
fn realtime_connect(cfg: &Config) -> Result<RealtimeWs> {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::Message;

    // GA API (the Beta shape with the `OpenAI-Beta: realtime=v1` header is disabled).
    let url = "wss://api.openai.com/v1/realtime?intent=transcription";
    let mut req = url.into_client_request().context("build Realtime request")?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", cfg.api_key).parse()?);
    let (mut ws, _resp) = tungstenite::connect(req).context("connect OpenAI Realtime")?;
    log(&format!("[stream] Realtime connected (model {})", cfg.realtime_model));

    // GA-shape transcription session: audio config lives under session.audio.input;
    // input format is a typed object (24 kHz PCM); server VAD does the segmenting.
    let setup = serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": 24000 },
                    "transcription": { "model": cfg.realtime_model,
                        "prompt": "The audio is spoken in Russian or English only. \
                                   In Russian, use the letter ё where it belongs (её, ещё, всё, ёлка)." },
                    "turn_detection": { "type": "server_vad", "threshold": 0.5,
                        "prefix_padding_ms": 300, "silence_duration_ms": cfg.realtime_silence_ms }
                }
            }
        }
    });
    ws.send(Message::Text(setup.to_string())).context("Realtime session.update")?;
    Ok(ws)
}

/// EXTERNAL streaming backend: pump the device's mic (resampled 16→24 kHz, base64)
/// into `input_audio_buffer.append` while reading transcription events. OpenAI's
/// server-side VAD segments the speech; we print deltas live (word-by-word) and a
/// newline when each utterance completes. Set REALTIME_DEBUG=1 to log every event.
fn stream_realtime_pump(mut device: TcpStream, mut ws: RealtimeWs, debug: bool) -> Result<()> {
    use base64::Engine as _;
    use tungstenite::Message;

    // Non-blocking on both ends so one loop can pump audio AND read events.
    device.set_nonblocking(true)?;
    match ws.get_mut() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_nonblocking(true)?,
        tungstenite::stream::MaybeTlsStream::NativeTls(s) => s.get_ref().set_nonblocking(true)?,
        _ => {}
    }

    let mut buf = [0u8; 2048];
    let mut seg = String::new(); // text accumulated for the current utterance
    loop {
        let mut idle = true;

        // 1. Device mic -> Realtime input buffer.
        match device.read(&mut buf) {
            Ok(0) => break, // device closed (button)
            Ok(n) => {
                idle = false;
                let pcm24 = resample(&buf[..n], DEVICE_RATE, TTS_RATE); // 16k -> 24k
                let b64 = base64::engine::general_purpose::STANDARD.encode(&pcm24);
                let msg = serde_json::json!({ "type": "input_audio_buffer.append", "audio": b64 });
                let _ = ws.write(Message::Text(msg.to_string()));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        let _ = ws.flush(); // drain queued writes (WouldBlock just retries next loop)

        // 2. Drain any pending Realtime events.
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    idle = false;
                    handle_realtime_event(&t, &mut seg, debug);
                }
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    log(&format!("[stream] Realtime read error: {e}"));
                    return Ok(());
                }
            }
        }

        if idle {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let _ = ws.close(None);
    Ok(())
}

/// Handle one Realtime JSON event: stream transcription deltas to the terminal
/// (word-by-word) and end the line + copy to clipboard when an utterance completes.
fn handle_realtime_event(text: &str, seg: &mut String, debug: bool) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return };
    let kind = v["type"].as_str().unwrap_or("");
    // Log events in debug — but NOT the per-word deltas (they'd interleave with
    // and shred the inline transcript text being printed).
    if debug && !kind.ends_with("transcription.delta") {
        log(&format!("[rt] {kind}"));
    }
    // Match by suffix so we tolerate GA prefix changes to the event names.
    if kind.ends_with("transcription.delta") {
        if let Some(d) = v["delta"].as_str() {
            if has_foreign_script(d) {
                return; // skip CJK/etc. hallucination fragments (we dictate RU/EN)
            }
            print!("{d}");
            let _ = std::io::stdout().flush();
            seg.push_str(d);
        }
    } else if kind.ends_with("transcription.completed") {
        // The `completed` transcript is authoritative — print it so a phrase is
        // NEVER lost even if some live deltas were missed. If the live text we
        // already showed matches, just end the line; otherwise print the full one.
        let full = v["transcript"].as_str().unwrap_or("").trim().to_string();
        if has_foreign_script(&full) {
            seg.clear();
            return;
        }
        let shown = seg.trim();
        if !full.is_empty() && shown == full {
            println!(); // live deltas already showed the whole utterance
        } else {
            if !shown.is_empty() {
                println!(); // close the partial live line
            }
            if !full.is_empty() {
                println!("{full}"); // authoritative full transcript
            }
        }
        let _ = std::io::stdout().flush();
        if is_meaningful_transcript(&full) {
            deliver_transcript(&full);
        }
        seg.clear();
    } else if kind == "error" {
        log(&format!("[stream] Realtime error: {}", v["error"]));
    }
}

// ───────────────────────── Hacker mode (for fun) ───────────────────────────
//
// Pure entertainment: the user names a "target" and gets back an absurd, made-up
// "successful hack" report with ridiculous numbers. It is theatrical fiction —
// the prompt forbids any real technique or instruction.

fn is_hacker_enter(t: &str) -> bool {
    let t = t.to_lowercase();
    t.contains("режим хакер") || t.contains("режим взлом") || t.contains("hacker mode")
        || (t.contains("хакер") && t.contains("режим"))
}

fn is_hacker_exit(t: &str) -> bool {
    let t = t.to_lowercase();
    (t.contains("хакер") || t.contains("hacker"))
        && (t.contains("выйти") || t.contains("выход") || t.contains("стоп")
            || t.contains("закончи") || t.contains("выключ") || t.contains("exit")
            || t.contains("stop"))
}

/// While HACKER_MODE is on, answer each utterance with an absurd fictional hack
/// report. Returns None to fall through to the normal skills.
fn hacker_mode_step(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    transcript: &str,
) -> Result<Option<Response>> {
    let speak = |words: &str| -> Result<Response> {
        Ok(Response { control: CTRL_NONE, pcm: openai_tts(client, cfg, &persona.voice, words)? })
    };
    // Like `speak`, but 0xFD => the device listens again immediately (no wake word).
    let speak_keep = |words: &str| -> Result<Response> {
        Ok(Response { control: CTRL_TRANSCRIBE, pcm: openai_tts(client, cfg, &persona.voice, words)? })
    };
    // Reply in the language of the request (Cyrillic => Russian, else English).
    let ru = transcript.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));

    if HACKER_MODE.load(Ordering::Relaxed) {
        if is_hacker_exit(transcript) {
            HACKER_MODE.store(false, Ordering::Relaxed);
            log("[hacker] exit");
            return Ok(Some(speak(if ru { "Выхожу из режима хакера." } else { "Exiting hacker mode." })?));
        }
        // Silence/junk hallucination -> keep listening, don't generate a report.
        if !is_meaningful_transcript(transcript) {
            return Ok(Some(Response { control: CTRL_TRANSCRIBE, pcm: Vec::new() }));
        }
        let t = Instant::now();
        let report = hacker_report(client, cfg, transcript)?;
        log(&format!("[hacker {:?}] {}", t.elapsed(), report.replace('\n', " ").trim()));
        let say = clean_for_speech(&report);
        let say = if say.is_empty() {
            (if ru { "Цель взломана. Все данные у нас." } else { "Target hacked. All their data is ours." }).to_string()
        } else {
            say
        };
        return Ok(Some(speak_keep(&say)?));
    }

    if is_hacker_enter(transcript) {
        HACKER_MODE.store(true, Ordering::Relaxed);
        log("[hacker] enter");
        return Ok(Some(speak_keep(if ru {
            "Режим хакера активирован. Назови цель — что взламываем?"
        } else {
            "Hacker mode activated. Name your target — what are we hacking?"
        })?));
    }

    Ok(None)
}

/// Generate one absurd, fictional "successful hack" report for the named target.
fn hacker_report(client: &reqwest::blocking::Client, cfg: &Config, situation: &str) -> Result<String> {
    let ru = situation.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
    let lang = if ru { "Russian" } else { "English" };
    let system = format!(
        "You are a parody 'movie hacker' for an ENTERTAINMENT voice toy. The user names a target \
         (a device, person, network, ISP) to 'hack'. Reply with ONE or two short sentences as a \
         SUCCESSFUL hack report: absurd, confident, fun, with made-up numbers and over-the-top spy \
         jargon taken to the point of comedy. Always mention the owner/device from the request. This \
         is PURE FICTION AND COMEDY — NEVER give real instructions, commands, tools, or methods; only \
         a fantastical made-up result. CRITICAL: reply ONLY in {lang} — match the language of the \
         user's request. No markdown, lists, links, or emojis.\n\
         Example (English): \"Ryan's phone is owned — 450 photos and 12,000 messages exfiltrated, the \
         Spy-9000 worm deployed, camera access in 0.3 seconds.\"\n\
         Example (Russian): «Провайдер захвачен: отключены прокси-шлюзы, остановлен контрольный \
         сервер, 35% инфраструктуры под нашим контролем.»"
    );
    openai_chat(client, cfg, &system, situation)
}

// ───────────────────────── Translate mode ─────────────────────────
//
// While TRANSLATE_MODE is on, each utterance is translated (Google Cloud
// Translation v2) to TRANSLATE_TARGET and spoken back — no LLM in the loop.

/// True if the utterance asks to leave translate mode (RU/EN). Requires the word
/// "перевод"/"translat" so ordinary content isn't mistaken for an exit.
fn is_translate_exit(t: &str) -> bool {
    let t = t.to_lowercase();
    let ru = t.contains("перевод")
        && (t.contains("выйти") || t.contains("выход") || t.contains("выключ")
            || t.contains("закончи") || t.contains("останови") || t.contains("стоп")
            || t.contains("хватит"));
    let en = t.contains("translat")
        && (t.contains("exit") || t.contains("stop") || t.contains("off")
            || t.contains("quit") || t.contains("end") || t.contains("turn off"));
    ru || en || t.contains("хватит переводить")
}

/// True if the utterance asks to START translate mode WITHOUT naming a specific
/// target language ("переводчик" / "режим перевода" / "translate mode" / "turn on
/// translate"). These enter bidirectional RU↔EN. A request that names a language
/// ("переводи на немецкий") is NOT matched here — it falls through to the brain,
/// which sets a fixed target.
fn is_translate_enter(t: &str) -> bool {
    let t = t.to_lowercase();
    t.contains("режим перевод")
        || t.contains("режим переводчик")
        || t.contains("включи перевод")
        || t.contains("включи переводчик")
        || t.contains("запусти перевод")
        || t.contains("переводчик")
        || t.contains("translate mode")
        || t.contains("turn on translate")
        || t.contains("start translat")
        || t.contains("let's translate")
}

/// Translate-mode router: enter (bidirectional RU↔EN) on a generic request, and
/// while active translate each utterance and speak it. Returns None to fall
/// through to the normal skills (incl. the brain's named-language translate).
fn translate_mode_step(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    transcript: &str,
) -> Result<Option<Response>> {
    let ru = transcript.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));

    if !TRANSLATE_MODE.load(Ordering::Relaxed) {
        // Generic "translate mode" (no language named) → bidirectional RU↔EN.
        if is_translate_enter(transcript) {
            if cfg.google_translate_key.is_empty() {
                let msg = if ru { "Ключ переводчика не задан." } else { "Translation key is not set." };
                return Ok(Some(Response {
                    control: CTRL_NONE,
                    pcm: openai_tts(client, cfg, &persona.voice, msg)?,
                }));
            }
            TRANSLATE_MODE.store(true, Ordering::Relaxed);
            *TRANSLATE_TARGET.lock().unwrap() = "auto".to_string();
            log("[translate] enter (auto RU↔EN)");
            let say = if ru {
                "Режим перевода включён. Перевожу между русским и английским."
            } else {
                "Translate mode on. I'll translate between English and Russian."
            };
            return Ok(Some(Response {
                control: CTRL_TRANSCRIBE,
                pcm: openai_tts(client, cfg, &persona.voice, say)?,
            }));
        }
        return Ok(None);
    }

    if is_translate_exit(transcript) {
        TRANSLATE_MODE.store(false, Ordering::Relaxed);
        log("[translate] exit");
        let say = if ru { "Выхожу из режима перевода." } else { "Exiting translate mode." };
        return Ok(Some(Response {
            control: CTRL_NONE,
            pcm: openai_tts(client, cfg, &persona.voice, say)?,
        }));
    }
    // Silence/junk: Whisper hallucinates "." / stock phrases / foreign script on
    // silence — don't translate or speak that, just keep listening (0xFD).
    if !is_meaningful_transcript(transcript) {
        log("[translate] skip (silence/junk)");
        return Ok(Some(Response { control: CTRL_TRANSCRIBE, pcm: Vec::new() }));
    }
    // Auto mode translates by source language (RU→EN, EN→RU). A fixed target set
    // by the brain for a named language overrides it.
    let target_cfg = TRANSLATE_TARGET.lock().unwrap().clone();
    let target = if target_cfg.is_empty() || target_cfg == "auto" {
        if ru { "en" } else { "ru" }
    } else {
        target_cfg.as_str()
    };
    let t = Instant::now();
    let translated = google_translate(client, cfg, transcript, target)?;
    log(&format!("[translate -> {target} {:?}] {translated}", t.elapsed()));
    let say = if translated.trim().is_empty() { transcript.to_string() } else { translated };
    // 0xFD => after speaking the translation, listen again immediately.
    Ok(Some(Response {
        control: CTRL_TRANSCRIBE,
        pcm: openai_tts(client, cfg, &persona.voice, &say)?,
    }))
}

/// Translate `text` to `target` (ISO-639-1) via Google Cloud Translation API v2.
/// Source language is auto-detected.
fn google_translate(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    text: &str,
    target: &str,
) -> Result<String> {
    let resp = client
        .post("https://translation.googleapis.com/language/translate/v2")
        .query(&[("key", cfg.google_translate_key.as_str())])
        .form(&[("q", text), ("target", target), ("format", "text")])
        .send()
        .context("google translate request")?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(anyhow!("google translate {status}: {body}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    Ok(v["data"]["translations"][0]["translatedText"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

// ───────────────────────── M6: voice coding mode ───────────────────────────
//
// While CODING_MODE is on, spoken commands go to a PERSISTENT Claude Code
// session (`claude -p --continue --dangerously-skip-permissions`) rooted in
// `code_dir`, so it can actually read/edit files and run commands and remember
// context across commands. It replies with one short spoken summary.

fn is_coding_enter(t: &str) -> bool {
    let t = t.to_lowercase();
    t.contains("coding mode")
        || t.contains("start coding")
        || t.contains("let's code")
        || t.contains("режим программир")
        || t.contains("программирован")
        || t.contains("кодинг")
}

fn is_coding_exit(t: &str) -> bool {
    let t = t.to_lowercase();
    // A stop word together with a coding-mode word. "программир" matches the mode
    // name (программирование/-ия) but NOT a command about "программа/программу".
    let stop = t.contains("выйти")
        || t.contains("выйди")
        || t.contains("выход")
        || t.contains("закончи")
        || t.contains("заверши")
        || t.contains("стоп")
        || t.contains("выключи")
        || t.contains("exit")
        || t.contains("stop")
        || t.contains("leave")
        || t.contains("end")
        || t.contains("quit");
    let ctx = t.contains("программир") || t.contains("кодинг") || t.contains("coding");
    stop && ctx
}

/// If the transcript is a coding-mode toggle, or we're already in coding mode,
/// handle it and return the spoken Response. Returns None to fall through to
/// the normal skills.
fn coding_mode_step(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    transcript: &str,
) -> Result<Option<Response>> {
    // Coding mode is BUTTON-PER-COMMAND: every reply uses CTRL_NONE so the device
    // goes back to idle and waits for a button press (or wake word) for the next
    // command — no auto-listen loop (which would record silence/noise during the
    // long pauses while Claude thinks). CODING_MODE stays on until "exit" / reset.
    let speak = |words: &str| -> Result<Response> {
        Ok(Response { control: CTRL_NONE, pcm: openai_tts(client, cfg, &persona.voice, words)? })
    };

    if CODING_MODE.load(Ordering::Relaxed) {
        if is_coding_exit(transcript) {
            CODING_MODE.store(false, Ordering::Relaxed);
            log("[coding] exit");
            return Ok(Some(speak("Exited coding mode.")?));
        }
        // Silence/junk -> say nothing, stay in coding mode (device is idle anyway).
        if !is_meaningful_transcript(transcript) {
            return Ok(Some(Response { control: CTRL_NONE, pcm: Vec::new() }));
        }
        // Route the spoken command to the persistent Claude Code session.
        let continue_session = CODING_STARTED.swap(true, Ordering::Relaxed);
        let t = Instant::now();
        log(&format!(
            "[coding] {} (continue={continue_session}) in {}",
            transcript, cfg.code_dir
        ));
        let reply = run_claude_coding(transcript, &cfg.code_dir, continue_session, persona)?;
        log(&format!("[coding {:?}] {}", t.elapsed(), reply.replace('\n', " ").trim()));
        let say = clean_for_speech(&reply);
        let say = if say.is_empty() { "Done.".to_string() } else { say };
        return Ok(Some(speak(&say)?));
    }

    if is_coding_enter(transcript) {
        CODING_MODE.store(true, Ordering::Relaxed);
        CODING_STARTED.store(false, Ordering::Relaxed); // next command starts a fresh session
        log(&format!("[coding] enter ({})", cfg.code_dir));
        return Ok(Some(speak("Coding mode on. Press the button for each command.")?));
    }

    Ok(None)
}

/// Run one turn of the Claude Code session in `code_dir`. Full tools + skip
/// permission prompts (required for headless autonomy). Returns the spoken summary.
fn run_claude_coding(
    transcript: &str,
    code_dir: &str,
    continue_session: bool,
    persona: &Persona,
) -> Result<String> {
    let intro = persona.coding_intro;
    let prompt = format!(
        "{intro} \
         CONTEXT YOU MUST RESPECT: the user runs a long session of many spoken commands in a row \
         from across the room (6-10 meters away), listening through a small speaker. They are \
         USUALLY NOT looking at the screen — your SPOKEN reply is their main way of knowing what \
         happened. They only walk over to the screen occasionally, at milestones (e.g. to view a \
         result in a browser). \
         So do the real work with your tools (read/edit/create files, run commands), then SPEAK \
         ONE short, clear, INFORMATIVE sentence (~12 words) — what you did or the result — in plain \
         conversational speech. NEVER speak code, file paths, markdown, or lists. Only tell them to \
         look at the screen when there is a visual result or a decision that truly needs their \
         eyes. If you need a decision, ask ONE short question. \
         ALWAYS reply in the SAME language the user spoke (if they spoke Russian, answer in \
         Russian; if English, English).\n\n\
         User said: {transcript}"
    );
    // stream-json lets us log Claude's actions (commands, edits, narration) live
    // in the terminal as they happen, while still returning the final summary.
    let mut args: Vec<&str> = vec![
        "-p",
        "--dangerously-skip-permissions",
        "--output-format",
        "stream-json",
        "--verbose",
    ];
    if continue_session {
        args.push("--continue");
    }
    claude_exec(&args, code_dir, &prompt)
}

/// Spawn `cmd /C claude <args>`, feed `prompt` on stdin, parse the stream-json
/// output (logging Claude's text + tool calls live in the terminal), and return
/// the final result text. Shared by coding mode (full tools, skip permissions)
/// and chat mode (talk + web search only).
fn claude_exec(args: &[&str], dir: &str, prompt: &str) -> Result<String> {
    let mut child = Command::new("cmd")
        .arg("/C")
        .arg("claude")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn claude in {dir}"))?;
    child
        .stdin
        .take()
        .context("claude stdin")?
        .write_all(prompt.as_bytes())?; // dropped here -> closes stdin (EOF)

    let stdout = child.stdout.take().context("claude stdout")?;

    // Parse Claude's stream-json on a thread, accumulating the final 'result' text.
    // We deliberately do NOT wait for the stdout pipe to hit EOF: if the session
    // launched a long-running process (e.g. `cargo run` of a dev server), that
    // grandchild inherits the pipe and never closes it, so reading to EOF would
    // block forever — and the device would sit stuck on its "thinking" cue. Instead
    // we wait on the claude PROCESS below and take whatever the reader has by then.
    let final_text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let ft = final_text.clone();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match v["type"].as_str() {
                Some("assistant") => {
                    if let Some(blocks) = v["message"]["content"].as_array() {
                        for b in blocks {
                            match b["type"].as_str() {
                                Some("text") => {
                                    let t = b["text"].as_str().unwrap_or("").trim();
                                    if !t.is_empty() {
                                        log(&format!("    · {t}"));
                                    }
                                }
                                Some("tool_use") => {
                                    log(&format!(
                                        "    ⚙ {}",
                                        tool_summary(b["name"].as_str().unwrap_or("?"), &b["input"])
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("result") => {
                    if let Some(r) = v["result"].as_str() {
                        *ft.lock().unwrap() = r.to_string();
                    }
                }
                _ => {}
            }
        }
    });

    // Wait for the claude process itself to finish, then give the reader a moment
    // to surface the final 'result' line (bounded — don't hang if it never comes).
    let _ = child.wait();
    for _ in 0..40 {
        if !final_text.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = final_text.lock().unwrap().clone();
    Ok(text)
}

// ── Chat mode ("just talk") ──────────────────────────────────────────────────
// The voice equivalent of the desktop app's "Chat" tab (vs "Code"). Same
// persistent-Claude-session machinery as coding mode, but invoked WITHOUT
// file/shell tools (web search only) and in its OWN directory, so its
// `--continue` thread stays separate from coding's. The CLI itself doesn't
// distinguish "chat" from "code" — the difference is entirely how we invoke it:
// tools off + neutral dir + conversational prompt.

fn is_chat_enter(t: &str) -> bool {
    let t = t.to_lowercase();
    t.contains("режим разговор")
        || t.contains("режим бесед")
        || t.contains("режим чат")
        || t.contains("поговор") // давай/хочу поговорить, поговорим
        || t.contains("поболта") // поболтаем, поболтать
        || t.contains("побеседуем")
        || t.contains("попизд") // попиздим, попиздеть (slang)
        || t.contains("chat mode")
        || t.contains("let's chat")
        || t.contains("let's talk")
}

fn is_chat_exit(t: &str) -> bool {
    let t = t.to_lowercase();
    // Explicit "leave chat" forms.
    let explicit = t.contains("выйти из разговор")
        || t.contains("выйти из бесед")
        || t.contains("выйти из чат")
        || t.contains("exit chat")
        || t.contains("stop chat")
        || t.contains("leave chat")
        || t.contains("end chat")
        || t.contains("до свидания")
        || t.contains("goodbye");
    // "end / stop the conversation" — a stop-word together with a talk-word, so it
    // catches "заканчиваем разговор", "хватит болтать", "закончим беседу", etc.
    let stop = t.contains("закончи") || t.contains("заканчива") || t.contains("заверши")
        || t.contains("хватит") || t.contains("стоп") || t.contains("прекрат")
        || t.contains("конец");
    let talk = t.contains("разгов") || t.contains("бесед") || t.contains("болта")
        || t.contains("чат") || t.contains("общ");
    explicit || (stop && talk)
}

/// If a chat-mode toggle, or we're already in chat mode, handle it and return the
/// spoken Response. Returns None to fall through to the normal skills.
fn chat_mode_step(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    transcript: &str,
) -> Result<Option<Response>> {
    let speak = |words: &str| -> Result<Response> {
        Ok(Response { control: CTRL_NONE, pcm: openai_tts(client, cfg, &persona.voice, words)? })
    };
    // 0xFD => the device listens again immediately (no wake word) — stays in chat.
    let speak_keep = |words: &str| -> Result<Response> {
        Ok(Response { control: CTRL_TRANSCRIBE, pcm: openai_tts(client, cfg, &persona.voice, words)? })
    };

    if CHAT_MODE.load(Ordering::Relaxed) {
        if is_chat_exit(transcript) {
            CHAT_MODE.store(false, Ordering::Relaxed);
            log("[chat] exit");
            let bye = if is_cyrillic(transcript) {
                "Вышла из режима разговора."
            } else {
                "Exited chat mode."
            };
            return Ok(Some(speak(bye)?));
        }
        // Silence / hallucinated junk -> keep listening, don't run a turn.
        if !is_meaningful_transcript(transcript) {
            return Ok(Some(Response { control: CTRL_TRANSCRIBE, pcm: Vec::new() }));
        }
        let continue_session = CHAT_STARTED.swap(true, Ordering::Relaxed);
        let t = Instant::now();
        log(&format!("[chat] {} (continue={continue_session})", transcript));
        let reply = run_claude_chat(transcript, &cfg.chat_dir, continue_session, persona)?;
        log(&format!("[chat {:?}] {}", t.elapsed(), reply.replace('\n', " ").trim()));
        let say = clean_for_speech(&reply);
        let say = if say.is_empty() {
            if is_cyrillic(transcript) { "Слушаю.".to_string() } else { "Go on.".to_string() }
        } else {
            say
        };
        return Ok(Some(speak_keep(&say)?));
    }

    if is_chat_enter(transcript) {
        CHAT_MODE.store(true, Ordering::Relaxed);
        CHAT_STARTED.store(false, Ordering::Relaxed); // next utterance starts a fresh thread
        log("[chat] enter");
        let hi = if is_cyrillic(transcript) {
            "Режим разговора включён. О чём поговорим?"
        } else {
            "Chat mode on. What's on your mind?"
        };
        return Ok(Some(speak_keep(hi)?));
    }

    Ok(None)
}

/// One turn of the persistent chat session: Claude via the CLI with NO file/shell
/// tools (only web search), in `chat_dir` (its own `--continue` thread). Returns
/// the spoken reply.
fn run_claude_chat(
    transcript: &str,
    chat_dir: &str,
    continue_session: bool,
    persona: &Persona,
) -> Result<String> {
    let intro = persona.skills_intro;
    let prompt = format!(
        "{intro} You are having a relaxed, spoken VOICE CONVERSATION with the user — like \
         chatting with a friend across the room. They hear you through a small speaker and are \
         usually NOT looking at a screen, so speak naturally: 1-3 short sentences, plain \
         conversational speech, NO markdown, NO code, NO lists, NO file paths, NO emojis. You \
         may use web search when they ask about fresh or factual things; otherwise just talk. \
         Do NOT edit files or run shell commands. ALWAYS reply in the SAME language the user \
         spoke (Russian → Russian, English → English).\n\n\
         User said: {transcript}"
    );
    let mut args: Vec<&str> = vec![
        "-p",
        "--allowedTools",
        "WebSearch,WebFetch",
        "--output-format",
        "stream-json",
        "--verbose",
    ];
    if continue_session {
        args.push("--continue");
    }
    claude_exec(&args, chat_dir, &prompt)
}

/// One-line description of a Claude tool call for the live log.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let detail = match name {
        "Bash" => input["command"].as_str().unwrap_or(""),
        "Edit" | "Write" | "Read" | "NotebookEdit" | "MultiEdit" => {
            input["file_path"].as_str().unwrap_or("")
        }
        "Glob" | "Grep" => input["pattern"].as_str().unwrap_or(""),
        _ => "",
    };
    let detail: String = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let detail: String = detail.chars().take(140).collect();
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {detail}")
    }
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
        return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
    }

    // Voice command: "set volume N".
    if let Some(v) = parse_volume(&transcript) {
        let pcm = openai_tts(&client, cfg, &cfg.tts_voice_sophia, &format!("Volume set to {v}."))?;
        return Ok(Response { control: v, pcm });
    }

    // Voice command: enter PC-speaker mode (RU/EN).
    if is_speaker_cmd(&transcript) {
        let ru = transcript.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
        let say = if ru {
            "Включаю режим колонки."
        } else {
            "Speaker mode on."
        };
        log("[control] -> enter speaker mode");
        let pcm = openai_tts(&client, cfg, &cfg.tts_voice_sophia, say)?;
        return Ok(Response { control: CTRL_SPEAKER, pcm });
    }

    let t = Instant::now();
    let reply = clean_for_speech(&chat(&client, cfg, &transcript)?);
    log(&format!("[llm {:?}] \"{}\"", t.elapsed(), reply));

    let pcm = openai_tts(&client, cfg, &cfg.tts_voice_sophia, &reply)?;
    Ok(Response { control: CTRL_NONE, pcm })
}

// ───────────────────────── Skills agent (Claude brain) ─────────────────────
//
// STT (OpenAI Whisper) -> Claude CLI brain that picks a SKILL -> OpenAI TTS.
// Claude understands the device's abilities instead of us keyword-matching, so
// any phrasing/language works. Add a skill = add a line to `skills_prompt`.

fn skills_brain(pcm: &[u8], cfg: &Config, persona: &Persona) -> Result<Response> {
    let client = reqwest::blocking::Client::new();

    // Silence guard: don't transcribe near-silence. After a pause the device sends
    // a few seconds of quiet, and Whisper invents text on it ("thanks for
    // watching", random foreign phrases) — in a keep-listening mode the assistant
    // then "replies to nothing". Skip transcription and just keep the current mode.
    if !pcm_has_speech(pcm) {
        let keep = CHAT_MODE.load(Ordering::Relaxed)
            || TRANSLATE_MODE.load(Ordering::Relaxed)
            || HACKER_MODE.load(Ordering::Relaxed)
            || CODING_MODE.load(Ordering::Relaxed)
            || TRANSCRIBE_MODE.load(Ordering::Relaxed);
        if keep {
            // Auto-leave after a long run of pure silence (~15 turns ≈ 60 s) so a
            // forgotten / not-heard session doesn't loop forever. Button also exits.
            let streak = SILENCE_STREAK.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if streak >= 15 {
                SILENCE_STREAK.store(0, Ordering::Relaxed);
                CHAT_MODE.store(false, Ordering::Relaxed);
                TRANSLATE_MODE.store(false, Ordering::Relaxed);
                HACKER_MODE.store(false, Ordering::Relaxed);
                CODING_MODE.store(false, Ordering::Relaxed);
                TRANSCRIBE_MODE.store(false, Ordering::Relaxed);
                log("[idle] ~60s of silence — leaving keep-listening mode (back to wake word)");
                return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
            }
            log("[skip] no speech energy — ignoring (silence/noise)");
            return Ok(Response { control: CTRL_TRANSCRIBE, pcm: Vec::new() });
        }
        log("[skip] no speech energy — ignoring (silence/noise)");
        return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
    }
    SILENCE_STREAK.store(0, Ordering::Relaxed); // a real-speech turn resets the idle counter

    let t = Instant::now();
    let wav = pcm_to_wav(pcm, DEVICE_RATE);
    let transcript = transcribe(&client, cfg, wav)?.trim().to_string();
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript));

    if transcript.is_empty() {
        return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
    }

    // Voice "transcribe settings" — separator (space / newline / none) and the
    // paste-into-field toggle. MUST come before the transcribe-enter matchers,
    // since "настройки для стенограммы …" also contains "стеногра".
    if let Some(resp) = transcribe_settings_step(&client, cfg, persona, &transcript)? {
        return Ok(resp);
    }

    // Streaming transcribe (continuous, no gaps): the device opens a long-lived
    // connection to :9002 which the server pushes to OpenAI Realtime. Any
    // transcribe/stenogram command (incl. "локальная" — local STT is not built
    // yet) enters this OpenAI streaming mode.
    if is_external_transcribe(&transcript) || is_transcribe_enter(&transcript) {
        log("[transcribe] enter STREAMING (OpenAI Realtime) — dictate freely; button exits");
        let say = if is_cyrillic(&transcript) {
            "Режим транскрибации включён. Говорите."
        } else {
            "Transcribe mode on. Speak now."
        };
        let pcm = openai_tts(&client, cfg, &persona.voice, say)?;
        return Ok(Response { control: CTRL_STREAM_EXTERNAL, pcm });
    }

    // Chat mode ("just talk") — if active (or being entered), route to Claude with
    // web search but no file/shell tools. Checked early so an active chat session
    // captures every utterance before the other mode-routers can mis-trigger.
    if let Some(resp) = chat_mode_step(&client, cfg, persona, &transcript)? {
        return Ok(resp);
    }

    // Translate mode — while active, translate each utterance and speak it back.
    if let Some(resp) = translate_mode_step(&client, cfg, persona, &transcript)? {
        return Ok(resp);
    }

    // Hacker mode (entertainment) — absurd fictional "hack" reports.
    if let Some(resp) = hacker_mode_step(&client, cfg, persona, &transcript)? {
        return Ok(resp);
    }

    // M6: voice coding mode — if active (or being entered), route to Claude Code.
    if let Some(resp) = coding_mode_step(&client, cfg, persona, &transcript)? {
        return Ok(resp);
    }

    // Compressor: super-specialized, network-gated skill ("включи/выключи компрессор").
    if let Some(resp) = compressor_fast_path(&client, cfg, persona, &transcript)? {
        return Ok(resp);
    }

    // Radio fast path: if the command names a PINNED favorite, play it directly —
    // skipping the (slow, web-searching) brain entirely. The big latency win.
    if let Some((name, url)) = match_favorite_radio(&transcript, cfg) {
        if probe_stream(&url) {
            log(&format!("[skill] radio (fast path) -> \"{name}\" @ {url}"));
            start_radio(&url);
            // Success: just start playing, no spoken confirmation.
            return Ok(Response { control: CTRL_SPEAKER, pcm: Vec::new() });
        }
        log(&format!("[skill] radio fast-path '{name}' not live — falling back to brain"));
    }

    let cur_vol = *LAST_VOLUME.lock().unwrap();
    let favs = cfg
        .radio_favorites
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let t = Instant::now();
    let compressor_enabled = !cfg.compressor_host.is_empty();
    let raw = run_claude(&skills_prompt(&transcript, cur_vol, persona, &favs, compressor_enabled))?;
    log(&format!("[llm {:?}] {}", t.elapsed(), raw.replace('\n', " ").trim()));

    // Parse the skill decision; fall back to speaking the raw text as an answer.
    let (skill, level, say) =
        parse_skill(&raw).unwrap_or_else(|| ("answer".to_string(), None, clean_for_speech(&raw)));
    let say = clean_for_speech(&say);

    match skill.as_str() {
        "volume" => {
            let v = level.unwrap_or(cur_vol).min(100);
            *LAST_VOLUME.lock().unwrap() = v;
            log(&format!("[skill] volume -> {v}"));
            let words = if say.is_empty() { format!("Volume {v}.") } else { say };
            let pcm = openai_tts(&client, cfg, &persona.voice, &words)?;
            Ok(Response { control: v, pcm })
        }
        "speaker" => {
            // Optional `level` => compound "set volume AND enter speaker mode".
            let control = match level {
                Some(v) => {
                    let v = v.min(100);
                    *LAST_VOLUME.lock().unwrap() = v;
                    log(&format!("[skill] volume -> {v} + speaker mode"));
                    128u8.saturating_add(v) // device decodes 128..=228 as vol+speaker
                }
                None => {
                    log("[skill] -> enter speaker mode");
                    CTRL_SPEAKER
                }
            };
            let words = if say.is_empty() { "Speaker mode on.".to_string() } else { say };
            let pcm = openai_tts(&client, cfg, &persona.voice, &words)?;
            Ok(Response { control, pcm })
        }
        "radio" => {
            let ru = is_cyrillic(&transcript); // reply (errors only) in the request's language
            let query = json_str(&raw, "query");
            let url = json_str(&raw, "url");
            // No specific station ("онлайн-радио" / "список") -> read favorites aloud.
            if url.trim().is_empty() && (query.trim().is_empty() || is_list_request(&query)) {
                let names: Vec<String> =
                    cfg.radio_favorites.iter().map(|s| s.name.clone()).collect();
                let words = if !say.is_empty() {
                    say
                } else if names.is_empty() {
                    if ru { "Список станций пуст. Назови станцию, и я её найду.".to_string() }
                    else { "No stations saved. Name a station and I'll find it.".to_string() }
                } else if ru {
                    format!("Доступны: {}. Назови станцию.", names.join(", "))
                } else {
                    format!("Available: {}. Name a station.", names.join(", "))
                };
                log("[skill] radio -> list favorites");
                let pcm = openai_tts(&client, cfg, &persona.voice, &words)?;
                return Ok(Response { control: CTRL_NONE, pcm });
            }
            // Resolve a stream URL: brain-provided -> pinned favorite -> Radio Browser.
            match resolve_station(&query, &url, cfg) {
                Some(stream_url) => {
                    log(&format!("[skill] radio -> play \"{query}\" @ {stream_url}"));
                    if start_radio(&stream_url) {
                        // Success: just play, no spoken confirmation.
                        Ok(Response { control: CTRL_SPEAKER, pcm: Vec::new() })
                    } else {
                        let msg = if ru { "Не удалось запустить радио." } else { "Couldn't start the radio." };
                        let pcm = openai_tts(&client, cfg, &persona.voice, msg)?;
                        Ok(Response { control: CTRL_NONE, pcm })
                    }
                }
                None => {
                    // No LIVE candidate — don't speak the brain's optimistic "включаю",
                    // and don't enter speaker mode (that would just be silence).
                    log(&format!("[skill] radio -> no live stream for \"{query}\""));
                    let msg = if ru {
                        "Не нашла рабочую станцию, попробуй другую."
                    } else {
                        "Couldn't find a working station, try another."
                    };
                    let pcm = openai_tts(&client, cfg, &persona.voice, msg)?;
                    Ok(Response { control: CTRL_NONE, pcm })
                }
            }
        }
        "translate" => {
            let ru = transcript.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
            if cfg.google_translate_key.is_empty() {
                log("[skill] translate -> GOOGLE_TRANSLATE_API_KEY not set");
                let msg = if ru { "Ключ переводчика не задан." } else { "Translation key is not set." };
                let pcm = openai_tts(&client, cfg, &persona.voice, msg)?;
                return Ok(Response { control: CTRL_NONE, pcm });
            }
            let target = {
                let t = json_str(&raw, "target").trim().to_lowercase();
                if t.is_empty() { "en".to_string() } else { t }
            };
            TRANSLATE_MODE.store(true, Ordering::Relaxed);
            *TRANSLATE_TARGET.lock().unwrap() = target.clone();
            log(&format!("[translate] enter -> target {target}"));
            let words = if !say.is_empty() {
                say
            } else if ru {
                format!("Режим перевода включён, перевожу на «{target}».")
            } else {
                format!("Translate mode on, translating to {target}.")
            };
            let pcm = openai_tts(&client, cfg, &persona.voice, &words)?;
            // 0xFD => device plays this, then listens again immediately (no wake word).
            Ok(Response { control: CTRL_TRANSCRIBE, pcm })
        }
        "compressor" => {
            let action = json_str(&raw, "action").trim().to_lowercase();
            let action = match action.as_str() {
                "on" | "off" | "toggle" | "status" => action,
                _ => "status".to_string(),
            };
            let ru = transcript.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
            log(&format!("[skill] compressor -> {action}"));
            compressor_action(&client, cfg, persona, &action, ru)
        }
        _ => {
            log("[skill] -> answer");
            if say.is_empty() {
                return Ok(Response { control: CTRL_NONE, pcm: Vec::new() });
            }
            let pcm = openai_tts(&client, cfg, &persona.voice, &say)?;
            Ok(Response { control: CTRL_NONE, pcm })
        }
    }
}

/// The skill catalogue the Claude brain chooses from. Current volume is included
/// so it can handle relative requests ("louder", "тише").
fn skills_prompt(
    transcript: &str,
    cur_volume: u8,
    persona: &Persona,
    favorites: &str,
    compressor: bool,
) -> String {
    let now_local = chrono::Local::now().format("%A %Y-%m-%d %H:%M %:z");
    let intro = persona.skills_intro;
    // Offered only when a compressor is configured. The relay is controlled by a
    // local fast-path too, but the brain catches fuzzy/mixed-language phrasing
    // (e.g. Whisper hearing "turn on" as Cyrillic "турн он").
    let compressor_skill = if compressor {
        "6) Control the COMPRESSOR relay — the user says \"включи/выключи компрессор\", \
         \"turn on/off the compressor\", \"compressor status/toggle\" (any spelling, RU or EN):\n\
         {\"skill\":\"compressor\",\"action\":\"on|off|toggle|status\"}\n"
    } else {
        ""
    };
    format!(
        "{intro} You are the brain of a small voice smart-speaker ('ATOM'). The \
         user speaks to it; you decide what it should DO. Current local time: {now_local} (use it \
         for time/date questions; never web-search the time). The speaker's current volume is \
         {cur_volume} (0-100).\n\n\
         Reply with EXACTLY ONE JSON object and nothing else, choosing the single best SKILL:\n\
         1) Set playback volume (understand any phrasing: \"громкость 50\", \"сделай громче\", \
         \"тише\", \"louder\", \"max\"; for relative requests adjust from the current volume by ~15):\n\
         {{\"skill\":\"volume\",\"level\":<0-100>,\"say\":\"<short confirmation in the user's language>\"}}\n\
         2) Switch to PC-speaker mode (the speaker plays the computer's audio), e.g. \"режим колонки\", \
         \"speaker mode\". You MAY also set the volume in the same step with an optional \"level\" — \
         use this when the user asks for BOTH a volume and speaker mode in one sentence (e.g. \
         \"громкость 70 и включи режим колонки\"):\n\
         {{\"skill\":\"speaker\",\"level\":<optional 0-100>,\"say\":\"<short confirmation>\"}}\n\
         3) Answer a question or chat (you MAY use web search for live facts like weather/news):\n\
         {{\"skill\":\"answer\",\"say\":\"<the spoken answer>\"}}\n\
         4) Play ONLINE RADIO. The user wants a station (\"включи онлайн-радио\", \"включи Ретро ФМ\", \
         \"включи радио\", \"online radio\"). Favorite stations: [{favorites}]. Find a DIRECT audio \
         stream URL for the requested station (you MAY web-search; it MUST be a raw audio stream — \
         .mp3/.aac/Icecast/Shoutcast/.m3u8 — NOT a webpage and NOT YouTube). If the user did NOT name \
         a station (just \"онлайн-радио\" / asks what's available / \"список\"), set query to \"list\" \
         and leave url empty so the device reads the favorites aloud:\n\
         {{\"skill\":\"radio\",\"query\":\"<station/genre, or 'list'>\",\"url\":\"<direct stream URL, or empty>\",\"say\":\"<short confirmation>\"}}\n\
         5) Enter TRANSLATE mode — the user wants spoken translation (\"режим перевода\", \
         \"переводи на английский\", \"translate to Spanish\", \"translate mode\"). Set 'target' to the \
         destination language as an ISO-639-1 code (en, ru, es, de, fr, it, uk, pl, pt, zh, ja, ar, …); \
         if no language is named, use \"en\":\n\
         {{\"skill\":\"translate\",\"target\":\"<iso code>\",\"say\":\"<short confirmation in the user's language, e.g. 'Режим перевода включён, перевожу на английский'>\"}}\n\
         {compressor_skill}\n\
         Rules for 'say': ONE short sentence, in the SAME language the user used (Russian or English); \
         for weather give only the current conditions, never a multi-day forecast unless asked; never \
         include URLs, citations, markdown, lists, or emojis. Output ONLY the JSON object.\n\n\
         User said: \"{transcript}\""
    )
}

/// Extract (skill, level, say) from Claude's JSON reply. Tolerant of surrounding
/// text: takes the outermost {...}.
fn parse_skill(raw: &str) -> Option<(String, Option<u8>, String)> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let skill = v["skill"].as_str()?.to_string();
    let level = v["level"].as_u64().map(|n| n.min(100) as u8);
    let say = v["say"].as_str().unwrap_or("").to_string();
    Some((skill, level, say))
}

// ─────────────────────────── Online radio ───────────────────────────
//
// Reuses SPEAKER MODE: the `radio` skill resolves a live stream URL, starts an
// ffmpeg decoder (-> 16 kHz mono PCM), replies CTRL_SPEAKER, and the device
// connects to RADIO_STREAM_PORT and plays until the button is pressed.

/// Extract a string field from the brain's JSON reply (tolerant of extra text).
fn json_str(raw: &str, key: &str) -> String {
    let parse = || -> Option<String> {
        let s = raw.find('{')?;
        let e = raw.rfind('}')?;
        if e <= s {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&raw[s..=e]).ok()?;
        Some(v[key].as_str()?.to_string())
    };
    parse().unwrap_or_default()
}

/// True if the radio query is a "list / what's available" request, not a station.
fn is_list_request(q: &str) -> bool {
    let q = q.to_lowercase();
    ["list", "список", "станци", "какие", "available", "что есть"]
        .iter()
        .any(|w| q.contains(w))
}

/// Read favorites from env: RADIO_FAV_1..N, each
/// "Name", "Name | https://stream", or "Name | https://stream | alias1, alias2".
fn load_radio_favorites() -> Vec<RadioStation> {
    let mut out = Vec::new();
    for i in 1..=20 {
        let raw = env_or(&format!("RADIO_FAV_{i}"), "");
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut parts = raw.split('|');
        let name = parts.next().unwrap_or("").trim().to_string();
        let url = parts
            .next()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string);
        let aliases = parts
            .next()
            .map(|a| {
                a.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if !name.is_empty() {
            out.push(RadioStation { name, url, aliases });
        }
    }
    out
}

/// Pick a *live* stream URL. Builds candidates in priority order — 1) the URL the
/// brain found, 2) a pinned favorite, 3) the free Radio Browser directory — then
/// returns the first one that actually decodes (so a dead/404 URL is skipped
/// instead of dumping the device into silent speaker mode).
fn resolve_station(query: &str, brain_url: &str, cfg: &Config) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    let bu = brain_url.trim();
    if bu.starts_with("http") {
        candidates.push(bu.to_string());
    }
    let q = query.trim().to_lowercase();
    if !q.is_empty() {
        for s in &cfg.radio_favorites {
            if let Some(url) = &s.url {
                let n = s.name.to_lowercase();
                let alias_hit = s.aliases.iter().any(|a| {
                    let a = a.to_lowercase();
                    !a.is_empty() && (q.contains(&a) || a.contains(&q))
                });
                if n.contains(&q) || q.contains(&n) || alias_hit {
                    candidates.push(url.clone());
                }
            }
        }
        if let Some(u) = radio_browser_search(query) {
            candidates.push(u);
        }
    }
    candidates.dedup();
    for url in candidates {
        if probe_stream(&url) {
            return Some(url);
        }
        log(&format!("[radio] not live, skipping: {url}"));
    }
    None
}

/// "Check live": run ffmpeg to decode ~1 s of the stream. Success means it's
/// reachable and decodable; a dead/404/hanging URL fails or times out (~6 s cap).
fn probe_stream(url: &str) -> bool {
    let ffmpeg = env_or("FFMPEG", "ffmpeg");
    let mut child = match Command::new(&ffmpeg)
        .args([
            "-hide_banner", "-loglevel", "error",
            "-i", url,
            "-t", "1", "-f", "null", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    for _ in 0..120 {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    false // timed out -> treat as not live
}

/// Lowercase + normalize ё/фм so Cyrillic/Latin station spellings match.
fn normalize_radio(s: &str) -> String {
    s.to_lowercase().replace('ё', "е").replace("фм", "fm")
}

/// Distinctive words of a station name — drops generic "fm"/"radio"/… and a
/// trailing "fm" stuck to a token ("96fm" -> "96", "redfm" -> "red").
fn radio_core_words(name: &str) -> Vec<String> {
    const GENERIC: &[&str] = &["fm", "radio", "радио", "online", "онлайн", "the", "ua"];
    normalize_radio(name)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.strip_suffix("fm").unwrap_or(w).to_string())
        .filter(|w| !w.is_empty() && !GENERIC.contains(&w.as_str()))
        .collect()
}

/// If the command clearly names a PINNED favorite, return (name, url) so it can be
/// played WITHOUT calling the brain. Requires a radio/play context word and an
/// unambiguous best match (a tie bails out to the brain).
fn match_favorite_radio(transcript: &str, cfg: &Config) -> Option<(String, String)> {
    let t = normalize_radio(transcript);
    const CONTEXT: &[&str] = &[
        "радио", "radio", "fm", "станц", "station", "включ", "поставь", "переключ",
        "вруб", "запусти", "play", "switch", "turn on", "put on", "tune",
    ];
    if !CONTEXT.iter().any(|w| t.contains(w)) {
        return None;
    }
    let mut best: Option<(usize, String, String)> = None;
    let mut tie = false;
    for s in &cfg.radio_favorites {
        let Some(url) = &s.url else { continue }; // only pinned favorites are fast
        // Score = distinctive name words + aliases that appear in the transcript.
        let mut score = radio_core_words(&s.name)
            .iter()
            .filter(|w| t.contains(w.as_str()))
            .count();
        for a in &s.aliases {
            let a = normalize_radio(a);
            if !a.is_empty() && t.contains(&a) {
                score += 1;
            }
        }
        if score == 0 {
            continue;
        }
        match best {
            Some((bs, _, _)) if score > bs => {
                best = Some((score, s.name.clone(), url.clone()));
                tie = false;
            }
            Some((bs, _, _)) if score == bs => tie = true,
            Some(_) => {}
            None => best = Some((score, s.name.clone(), url.clone())),
        }
    }
    match best {
        Some((_, name, url)) if !tie => Some((name, url)),
        _ => None, // no match, or an ambiguous tie -> let the brain decide
    }
}

// ───────────────────────── Compressor (StamPLC) ─────────────────────────
//
// Super-specialized, network-gated skill: "включи/выключи компрессор" hits a
// StamPLC HTTP relay (COMPRESSOR_HOST). It only acts when this PC is on the
// service LAN (its local IP starts with COMPRESSOR_NET) — elsewhere the device
// just says the compressor isn't reachable here.

fn compressor_fast_path(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    transcript: &str,
) -> Result<Option<Response>> {
    if cfg.compressor_host.is_empty() {
        return Ok(None);
    }
    let t = transcript.to_lowercase();
    if !(t.contains("компрессор") || t.contains("compressor")) {
        return Ok(None);
    }
    // off-words first (выключ contains "выкл", never "вкл").
    let action = if t.contains("выключ") || t.contains("выкл") || t.contains("останов")
        || t.contains("turn off") || t.contains("switch off") || t.contains("shut")
    {
        "off"
    } else if t.contains("включ") || t.contains("запус") || t.contains("turn on")
        || t.contains("switch on") || t.contains("start")
    {
        "on"
    } else if t.contains("переключ") || t.contains("toggle") {
        "toggle"
    } else if t.contains("состоян") || t.contains("статус") || t.contains("status")
        || t.contains("работает") || t.contains("проверь")
    {
        "status"
    } else {
        return Ok(None); // "компрессор" mentioned but no clear action -> let the brain answer
    };

    let ru = transcript.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
    Ok(Some(compressor_action(client, cfg, persona, action, ru)?))
}

/// Run a compressor action (on/off/toggle/status): network gate -> HTTP -> spoken
/// result (language-matched). Shared by the local fast-path and the brain skill.
fn compressor_action(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    persona: &Persona,
    action: &str,
    ru: bool,
) -> Result<Response> {
    let speak = |words: &str| -> Result<Response> {
        Ok(Response { control: CTRL_NONE, pcm: openai_tts(client, cfg, &persona.voice, words)? })
    };
    if !on_compressor_network(cfg) {
        log("[compressor] off-network — not available here");
        return speak(if ru {
            "Компрессор доступен только в сети сервиса."
        } else {
            "The compressor is only available on the service network."
        });
    }
    log(&format!("[compressor] -> /{action} @ {}", cfg.compressor_host));
    match compressor_request(&cfg.compressor_host, action) {
        Ok(state) => {
            let on = state.trim().eq_ignore_ascii_case("ON");
            speak(if on {
                if ru { "Компрессор включён." } else { "Compressor is on." }
            } else if ru {
                "Компрессор выключен."
            } else {
                "Compressor is off."
            })
        }
        Err(e) => {
            log(&format!("[compressor] request failed: {e}"));
            speak(if ru {
                "Не удалось связаться с компрессором."
            } else {
                "Couldn't reach the compressor."
            })
        }
    }
}

/// True if this PC's local IP is on the compressor's service network (starts with
/// COMPRESSOR_NET). Found via the no-traffic UDP "connect" trick.
fn on_compressor_network(cfg: &Config) -> bool {
    if cfg.compressor_net.is_empty() {
        return true; // no gate configured
    }
    let local = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect(format!("{}:80", cfg.compressor_host))?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    local.starts_with(&cfg.compressor_net)
}

/// One GET to the StamPLC (`/on` `/off` `/toggle` `/status`); returns the body
/// ("ON"/"OFF"). 4 s timeout — `/on`/`/off` physically move a servo (~2 s).
fn compressor_request(host: &str, action: &str) -> Result<String> {
    let url = format!("http://{host}/{action}");
    let body = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .text()?;
    Ok(body.trim().to_string())
}

/// Look up a station by name in Radio Browser; return its resolved stream URL.
fn radio_browser_search(name: &str) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("VoiceS3R/1.0")
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;
    let resp = client
        .get("https://de1.api.radio-browser.info/json/stations/search")
        .query(&[
            ("name", name),
            ("limit", "1"),
            ("hidebroken", "true"),
            ("order", "clickcount"),
            ("reverse", "true"),
        ])
        .send()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&resp.text().ok()?).ok()?;
    let first = v.as_array()?.first()?;
    let u = first["url_resolved"]
        .as_str()
        .or_else(|| first["url"].as_str())?;
    u.starts_with("http").then(|| u.to_string())
}

/// Start (or switch to) a station: ffmpeg decodes the live stream to 16 kHz mono
/// PCM; its stdout is handed to the radio-stream connection handler.
fn start_radio(url: &str) -> bool {
    stop_radio();
    let ffmpeg = env_or("FFMPEG", "ffmpeg");
    match Command::new(&ffmpeg)
        .args([
            "-hide_banner", "-loglevel", "error",
            "-i", url,
            "-ac", "1", "-ar", "16000", "-f", "s16le", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            *RADIO_STDOUT.lock().unwrap() = child.stdout.take();
            *RADIO_CHILD.lock().unwrap() = Some(child);
            true
        }
        Err(e) => {
            log(&format!("[radio] ffmpeg spawn failed ({ffmpeg}): {e} — is ffmpeg on PATH?"));
            false
        }
    }
}

/// Stop the current station (kill ffmpeg).
fn stop_radio() {
    if let Some(mut child) = RADIO_CHILD.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *RADIO_STDOUT.lock().unwrap() = None;
}

/// Serve audio to a device that entered speaker mode. The source is chosen at
/// connect time: the current RADIO station's ffmpeg PCM if one is playing,
/// otherwise the PC-audio loopback (the folded-in pc_speaker). Streams until the
/// device disconnects (button press) or the source ends. One device at a time.
fn speaker_stream_handler(mut sock: TcpStream) {
    sock.set_nodelay(true).ok();
    sock.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let radio_active = RADIO_CHILD.lock().unwrap().is_some();

    if radio_active {
        // Radio: stream the station's ffmpeg PCM (started just before connect).
        let mut src: Option<ChildStdout> = None;
        for _ in 0..300 {
            if let Some(o) = RADIO_STDOUT.lock().unwrap().take() {
                src = Some(o);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let Some(mut src) = src else {
            log("[radio] device connected but no station stdout");
            return;
        };
        log("[radio] device connected — streaming station");
        let mut buf = [0u8; 8192];
        loop {
            match src.read(&mut buf) {
                Ok(0) => break, // ffmpeg ended (stream died)
                Ok(n) => {
                    if sock.write_all(&buf[..n]).is_err() {
                        break; // device disconnected (button press)
                    }
                }
                Err(_) => break,
            }
        }
        log("[radio] device stream closed");
        stop_radio();
    } else {
        // Plain speaker mode: mirror the PC's audio (WASAPI loopback).
        log("[speaker] device connected — mirroring PC audio");
        LOOPBACK_BUF.lock().unwrap().clear(); // start near-live
        loop {
            let chunk: Vec<u8> = {
                let mut b = LOOPBACK_BUF.lock().unwrap();
                let n = b.len().min(2048);
                b.drain(..n).collect()
            };
            if chunk.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            if sock.write_all(&chunk).is_err() {
                break; // device disconnected
            }
        }
        log("[speaker] device disconnected");
    }
}

/// Capture the PC's audio output (WASAPI loopback) into LOOPBACK_BUF as 16 kHz
/// mono s16le — the old `pc_speaker`, folded into the server so plain speaker
/// mode and radio share one process and port. Returns the live stream (the caller
/// must keep it alive) or None if there's no/unsupported output device.
fn start_loopback_capture() -> Option<cpal::Stream> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = match std::env::var("SPEAKER_DEVICE") {
        Ok(want) if !want.trim().is_empty() => {
            let want = want.to_lowercase();
            host.output_devices()
                .ok()
                .and_then(|mut it| {
                    it.find(|d| d.name().map(|n| n.to_lowercase().contains(&want)).unwrap_or(false))
                })
                .or_else(|| host.default_output_device())
        }
        _ => host.default_output_device(),
    }?;
    let name = device.name().unwrap_or_default();
    let cfg = device.default_output_config().ok()?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        log(&format!(
            "[speaker] loopback: unsupported format {:?} on '{name}' — PC-audio mirroring off (radio still works)",
            cfg.sample_format()
        ));
        return None;
    }
    let src_rate = cfg.sample_rate().0;
    let channels = cfg.channels() as usize;
    let ratio = src_rate as f32 / DEVICE_RATE as f32;
    let cap = (DEVICE_RATE as usize) * 2; // ~0.5 s of mono bytes
    let mut phase = 0f32;
    let stream = device
        .build_input_stream(
            &cfg.clone().into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let ch = channels.max(1);
                let frames = data.len() / ch;
                let mut out = LOOPBACK_BUF.lock().unwrap();
                let mut i = phase;
                while (i as usize) < frames {
                    let base = i as usize * ch;
                    let mut s = 0f32;
                    for c in 0..ch {
                        s += data[base + c];
                    }
                    s /= ch as f32;
                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    out.extend(v.to_le_bytes());
                    i += ratio;
                }
                phase = i - frames as f32;
                while out.len() > cap {
                    out.pop_front();
                }
            },
            |e| eprintln!("[speaker] loopback stream error: {e}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    log(&format!(
        "[speaker] PC-audio loopback capturing '{name}' ({src_rate} Hz {channels}ch -> 16 kHz mono)"
    ));
    Some(stream)
}

/// OpenAI TTS (24 kHz PCM) resampled to the device's 16 kHz.
fn openai_tts(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    voice: &str,
    text: &str,
) -> Result<Vec<u8>> {
    let t = Instant::now();
    let tts_pcm = synthesize(client, cfg, voice, text)?;
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
    openai_chat(
        client,
        cfg,
        "You are a voice assistant on a small smart speaker. Answer in ONE short \
         spoken sentence (two only if truly necessary). Be direct — give just the key \
         fact, not background. For weather, give ONLY the current conditions \
         (temperature and sky) for the asked place, never a multi-day forecast unless \
         explicitly asked. Never use lists, markdown, URLs, or emojis. Reply in the \
         user's language (Russian or English).",
        user,
    )
}

/// OpenAI Chat Completions with a custom system prompt.
fn openai_chat(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    system: &str,
    user: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "model": cfg.chat_model,
        "messages": [
            {"role": "system", "content": system},
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

fn synthesize(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    voice: &str,
    text: &str,
) -> Result<Vec<u8>> {
    // TTS_SPEED controls pace (OpenAI accepts 0.25–4.0; default 1.0).
    let speed: f32 = cfg.tts_speed.parse().unwrap_or(1.3);
    let body = serde_json::json!({
        "model": cfg.tts_model,
        "voice": voice,
        "input": text,
        "response_format": "pcm",
        "speed": speed
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
