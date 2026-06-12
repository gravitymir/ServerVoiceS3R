//! ATOM VoiceS3R PC server.
//!
//! Protocol (raw TCP, one connection per utterance):
//!   - Device connects, streams 16 kHz / mono / 16-bit LE PCM, then half-closes
//!     its write side (EOF) to mark end of utterance.
//!   - We transcribe (OpenAI Whisper) -> chat reply (OpenAI) -> speech (OpenAI
//!     TTS, 24 kHz PCM) -> resample to 16 kHz -> stream back -> close.
//!
//! Config via env:
//!   OPENAI_API_KEY   (required)
//!   PORT             (default 9000)
//!   CHAT_MODEL       (default gpt-4o-mini)
//!   TTS_MODEL        (default gpt-4o-mini-tts)
//!   TTS_VOICE        (default alloy)
//!   STT_MODEL        (default whisper-1)

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

const DEVICE_RATE: u32 = 16_000; // device PCM sample rate
const TTS_RATE: u32 = 24_000; // OpenAI TTS pcm sample rate

struct Config {
    api_key: String,
    port: u16,
    chat_model: String,
    tts_model: String,
    tts_voice: String,
    stt_model: String,
    loopback: bool,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() -> Result<()> {
    let loopback = std::env::var("LOOPBACK").is_ok();
    let cfg = Config {
        api_key: if loopback {
            String::new()
        } else {
            std::env::var("OPENAI_API_KEY")
                .map_err(|_| anyhow!("set OPENAI_API_KEY (or set LOOPBACK=1 to echo-test)"))?
        },
        port: env_or("PORT", "9000").parse().unwrap_or(9000),
        chat_model: env_or("CHAT_MODEL", "gpt-4o-mini"),
        tts_model: env_or("TTS_MODEL", "gpt-4o-mini-tts"),
        tts_voice: env_or("TTS_VOICE", "alloy"),
        stt_model: env_or("STT_MODEL", "whisper-1"),
        loopback,
    };
    if loopback {
        log("LOOPBACK mode: echoing recorded audio back (no API calls)");
    }

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).with_context(|| format!("bind {addr}"))?;
    log(&format!("listening on {addr}"));
    log(&format!(
        "models: stt={} chat={} tts={} voice={}",
        cfg.stt_model, cfg.chat_model, cfg.tts_model, cfg.tts_voice
    ));
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
    stream
        .read_to_end(&mut pcm)
        .context("reading utterance PCM")?;
    let secs = pcm.len() as f32 / (DEVICE_RATE as f32 * 2.0);
    log(&format!(
        "[recv] {} bytes (~{:.1}s of 16kHz mono) in {:?}",
        pcm.len(),
        secs,
        t0.elapsed()
    ));
    if pcm.len() < 4000 {
        log("[skip] utterance too short, sending silence");
        return Ok(());
    }

    // Loopback: echo the recorded audio straight back (no API keys needed).
    if cfg.loopback {
        stream.write_all(&pcm).context("writing echo PCM")?;
        stream.flush().ok();
        log(&format!("[loopback] echoed {} bytes back", pcm.len()));
        return Ok(());
    }

    let client = reqwest::blocking::Client::new();

    // 2. Transcribe (Whisper).
    let t = Instant::now();
    let wav = pcm_to_wav(&pcm, DEVICE_RATE);
    let transcript = transcribe(&client, cfg, wav)?;
    log(&format!("[stt {:?}] \"{}\"", t.elapsed(), transcript.trim()));
    if transcript.trim().is_empty() {
        log("[skip] empty transcript");
        return Ok(());
    }

    // 3. Chat completion.
    let t = Instant::now();
    let reply = chat(&client, cfg, transcript.trim())?;
    log(&format!("[llm {:?}] \"{}\"", t.elapsed(), reply.trim()));

    // 4. Text-to-speech (24 kHz PCM) -> resample to 16 kHz.
    let t = Instant::now();
    let tts_pcm = synthesize(&client, cfg, reply.trim())?;
    let out_pcm = resample(&tts_pcm, TTS_RATE, DEVICE_RATE);
    log(&format!(
        "[tts {:?}] {} bytes @24k -> {} bytes @16k",
        t.elapsed(),
        tts_pcm.len(),
        out_pcm.len()
    ));

    // 5. Stream response back and close.
    stream.write_all(&out_pcm).context("writing response PCM")?;
    stream.flush().ok();
    log(&format!("[done] total {:?}", t0.elapsed()));
    Ok(())
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
                 natural, spoken style — usually 1-3 short sentences. No markdown or emojis."},
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
        "response_format": "pcm" // raw 24 kHz, mono, s16le
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

/// Wrap s16le mono PCM in a 44-byte WAV header.
fn pcm_to_wav(pcm: &[u8], rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = rate * 2;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
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
    // Seconds since process start are enough for a readable console trace.
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    println!("[{:7.2}s] {msg}", start.elapsed().as_secs_f32());
}
