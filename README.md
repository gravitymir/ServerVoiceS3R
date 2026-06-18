# ServerVoiceS3R

PC-side voice-assistant server for the **M5Stack ATOM VoiceS3R** firmware
([VoiceS3R](https://github.com/gravitymir/VoiceS3R)).

The device listens for two on-device wake words — **"Sophia"** and **"Jarvis"**
(or a button press) — records what you say, and streams a 1-byte persona id
(which name fired) followed by 16 kHz mono PCM to this server over raw TCP. The
server turns it into a spoken reply (and/or a device command) and streams the
audio back, which the device plays on its speaker. The persona picks the voice and
character: **Sophia** = female (`nova`), **Jarvis** = male (`onyx`).

```
ATOM VoiceS3R  ──(name / button)── [persona byte] + 16 kHz mono PCM ──TCP─▶  ServerVoiceS3R :9000
   speaker     ◀──────────────────────────────────  16 kHz mono PCM ──TCP──  STT → brain → TTS
```

## Two programs

This crate builds **two** executables:

| Binary | Port | Role | Needed for |
|---|---|---|---|
| `server_voice_s3r.exe` | 9000 | **The brain.** Speech → STT → reply/skill → TTS → spoken audio back. Also handles volume + speaker-mode control and voice coding. | Always |
| `pc_speaker.exe` | 9001 | **PC-audio streamer.** Captures the PC's audio output (WASAPI loopback) and streams it to the device so the ATOM plays your computer's sound. | Only for WiFi speaker mode |

Run just the main server for the assistant; run both if you also want the device
to act as a wireless speaker for the PC.

## Quick start (recommended: `skills` mode)

1. Build:
   ```powershell
   cargo build --release
   ```
2. Put a **`.env`** file next to the exe — `target\release\.env` — so you don't
   pass settings on the command line each run (the server reads it automatically;
   real environment variables override it):
   ```ini
   MODE=skills
   OPENAI_API_KEY=<your-openai-api-key>
   TTS_VOICE_SOPHIA=nova
   TTS_VOICE_JARVIS=onyx
   TTS_SPEED=1.4
   CODE_DIR=C:/Users/you/voice-code
   ```
3. Start it:
   ```powershell
   .\target\release\server_voice_s3r.exe
   # optional, for speaker mode:
   .\target\release\pc_speaker.exe
   ```
4. The device must be on the **same LAN** and provisioned with this PC's
   `IP:9000` (e.g. `192.168.8.100:9000`).

## Modes (`MODE` env var)

### `skills` (recommended) — smart agent, OpenAI voice
**OpenAI Whisper** (STT, cloud) → **Claude CLI** brain that picks a *skill* →
**OpenAI TTS** (e.g. the female `nova` voice). The brain understands the device:

- **answer** — speak a concise reply (may use web search for live facts).
- **volume** — set speaker volume ("set volume 50", "сделай громче").
- **speaker** — enter WiFi speaker mode ("режим колонки"); can also set volume
  in the same command.
- **coding mode** — see below.
- **transcribe mode** — see below.

**Dual persona.** The device sends a persona id (which wake word fired) as the
first request byte. Say **"Sophia"** → female persona, `TTS_VOICE_SOPHIA` (`nova`); say
**"Jarvis"** → male persona, `TTS_VOICE_JARVIS` (`onyx`). The brain and the coding
agent both adopt the matching character and grammatical gender for that turn. A
button press uses the default Sophia persona.

Needs `OPENAI_API_KEY` (STT + TTS) and the [`claude` CLI](https://claude.com/claude-code) on `PATH`.

### `windows` — fully local + Claude subscription (no API keys)
Local **Whisper** (`stt_server.py`, or `STT_ENGINE=sapi` for Windows
System.Speech) → `claude` CLI reply → **Windows SAPI** TTS.

### `openai` — all-OpenAI
OpenAI Whisper → Chat Completions → OpenAI TTS.

### `loopback` (`LOOPBACK=1`) — no AI
Echoes recorded audio back; proves the mic → TCP → speaker round-trip.

## Voice coding mode (M6)

In `skills` mode, say **"coding mode"** to route every spoken command to a
**persistent Claude Code session** rooted in `CODE_DIR`
(`claude -p --continue --dangerously-skip-permissions`). It can read/edit/create
files and run commands, and remembers context across commands. Say
**"exit coding mode"** to return to the normal assistant.

The agent is told it's a hands-free voice session — the user is across the room
and usually isn't watching the screen — so it replies with **one short, informative
sentence** and only points you to the screen at milestones (e.g. a browser result).

> ⚠️ **Autonomy / safety:** `--dangerously-skip-permissions` lets the agent run
> any file/shell action **without confirmation**. Point `CODE_DIR` only at a
> project you're comfortable letting voice commands modify (the default is a
> sandbox folder).

## Transcribe mode (voice → text)

Turns the device into a pure dictation tool. In `skills` mode, say the wake word
then **"режим транскрибации"** (or "transcribe mode" / "режим стенограммы" /
"voice to text" / "диктовка"). The device confirms *"Transcribe mode on. Speak
now."* and then **records sentence after sentence continuously — no wake word
between them**. Each utterance is transcribed, printed in the server terminal
(`📝 TRANSCRIPT`) and copied to the **Windows clipboard** — nothing is sent to the
LLM and nothing is spoken back, so you can paste it into any other app or website.

**Two ways out** (voice does not exit, so dictated text is never mistaken for a
command):
1. **Press the device button** (G41) — the device signals the server and you hear
   *"Transcribe mode off."*
2. **Idle timeout** — after `TRANSCRIBE_TIMEOUT` seconds (default **60**) with no
   speech, the server leaves the mode automatically.

After exiting, the device returns to normal **wake-word listening**. Each sentence
is printed in the terminal and copied to the clipboard (per sentence; no full-
session accumulation).

Set the timeout in `.env`: `TRANSCRIBE_TIMEOUT=60` (seconds).

### Streaming transcribe (continuous, no gaps)

The mode above re-connects per sentence. For fluid dictation there's a **streaming**
variant: the device opens one long-lived connection (port **9002**) and pushes the
mic continuously; the server segments it (server-side VAD) and transcribes each
segment off the read path, so nothing is lost between sentences. **Exit = button.**

Any transcribe/stenogram command ("стенограмма" / "транскрибация" / "voice to
text" / "диктовка") starts it. STT is the **OpenAI Realtime API** (websocket,
word-by-word, server-side VAD); if the websocket can't connect (bad/expired/
missing key or no network) it falls back to per-segment OpenAI Whisper REST.
Russian/English auto-detect, foreign-script hallucinations dropped. Exit with the
**button**.

> A fully local, offline STT engine (whisper.cpp in-process) is planned for a
> later build; for now transcription uses the OpenAI API.

## WiFi speaker mode

`pc_speaker.exe` (port 9001) loopback-captures the PC's **default output device**
and streams it to the device. Pick the device with `SPEAKER_DEVICE` (a name
substring) if the default isn't the one you hear:

```powershell
$env:SPEAKER_DEVICE="Speakers"   # optional; else uses the system default
.\target\release\pc_speaker.exe
```

Say "режим колонки" / "speaker mode" to the device to start; press the button to exit.

## Configuration

The server reads a **`.env`** file next to the exe (then `./.env`), `KEY=VALUE`
per line, `#` comments; real environment variables take precedence.

| Variable | Default | Purpose |
|---|---|---|
| `MODE` | `windows` | `skills` \| `windows` \| `openai` \| `loopback` |
| `OPENAI_API_KEY` | — | Required for `skills` and `openai` (STT + TTS) |
| `PORT` | `9000` | TCP listen port |
| `TTS_VOICE_SOPHIA` | `nova` | OpenAI TTS voice for the **Sophia** persona (`nova`, `shimmer`, …) |
| `TTS_VOICE_JARVIS` | `onyx` | OpenAI TTS voice for the **Jarvis** persona (`onyx`, `echo`, `ash`) |
| `TTS_SPEED` | `1.3` | Speech rate (0.25–4.0; higher = faster) |
| `CODE_DIR` | `C:/Users/gravi/voice-code` | Project folder for voice coding mode (M6) |
| `TRANSCRIBE_TIMEOUT` | `60` | Seconds of silence before (chunked) transcribe mode auto-exits |
| `REALTIME_MODEL` | `gpt-4o-transcribe` | OpenAI Realtime transcription model (streaming dictation) |
| `TYPE_INTO_FOCUS` | unset | Live dictation: paste each phrase (Ctrl+V) into the focused field |
| `REALTIME_SILENCE_MS` | `1500` | Realtime server-VAD silence (ms) before finalizing a phrase |
| `REALTIME_DEBUG` | unset | If `1`, log every Realtime websocket event |
| `STT_MODEL` | `whisper-1` | OpenAI transcription model (command recognition) |
| `CHAT_MODEL` | `gpt-4o-mini` | (openai mode) reply model |
| `TTS_MODEL` | `gpt-4o-mini-tts` | OpenAI speech model |
| `STT_ENGINE` | `whisper` | (windows mode) `whisper` \| `sapi` |
| `STT_URL` | `http://127.0.0.1:9100/stt` | (windows mode) local Whisper microservice endpoint |
| `SPEAKER_DEVICE` | (default device) | (pc_speaker) output device name substring |
| `LOOPBACK` | unset | If set, forces loopback mode |

## Protocol

One TCP connection per utterance:

1. The device streams **16 kHz, mono, 16-bit little-endian PCM**, then half-closes
   its write side (EOF) to mark the end of the utterance.
2. The server replies with a **1-byte control header** then response PCM:
   `0xFF` = no change · `0x00..=100` = set volume · `0xFE` = enter speaker mode ·
   `0xFD` = continuous transcribe · `0xFB` = start streaming transcribe (port 9002)
   · `128..=228` = set volume *and* speaker mode.
3. The device applies the control byte and plays the PCM.

The request's first byte is a header: low 7 bits = persona (wake word), high bit
`0x80` = "button: leave transcribe mode".

## Building

Pure Rust — no C/C++ toolchain, no Python:

```powershell
cargo build --release
```

(Transcription uses the OpenAI API. An optional offline whisper.cpp engine is
planned for later; it will add a C++/CMake build step when introduced.)

## Firewall

Allow the server through the Windows firewall (Private networks) the first time,
or pre-create the rule:

```powershell
New-NetFirewallRule -DisplayName "VoiceS3R 9000" -Direction Inbound -LocalPort 9000 -Protocol TCP -Action Allow
```

## License

MIT
