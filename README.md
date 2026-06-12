# ServerVoiceS3R

PC-side voice-assistant server for the **M5Stack ATOM VoiceS3R** firmware
([VoiceS3R](https://github.com/gravitymir/VoiceS3R)).

The device streams microphone audio to this server over raw TCP; the server turns
it into a spoken reply and streams the audio back, which the device plays on its
speaker.

```
ATOM VoiceS3R  ──(hold button)── 16 kHz mono PCM ──TCP──▶  ServerVoiceS3R
   speaker     ◀────────────────  16 kHz mono PCM ──TCP──   Whisper → GPT → TTS
```

## Protocol

One TCP connection per utterance:

1. The device connects and streams **16 kHz, mono, 16-bit little-endian PCM**
   while its button is held.
2. On button release the device half-closes its write side (EOF) to mark the end
   of the utterance.
3. The server transcribes → generates a reply → synthesizes speech, then streams
   **16 kHz mono s16le PCM** back and closes the connection.

## Modes

Selected with the `MODE` env var (default `windows`); `LOOPBACK=1` forces loopback.

### Windows (default — no API keys)

Fully local + your Claude subscription:

- **STT** — Windows `System.Speech` offline recognizer
- **Reply** — the `claude` CLI (`claude -p`, prompt piped on stdin)
- **TTS** — Windows SAPI (`System.Speech.Synthesis`), 16 kHz mono

```powershell
cargo run --release            # MODE defaults to windows
```

Requires the [`claude` CLI](https://claude.com/claude-code) on `PATH`. Round-trip
is ~6–10 s. STT accuracy is modest (the built-in recognizer); swap in local
Whisper later for better transcription.

### Loopback (no API key)

Echoes the recorded audio straight back — proves the full
mic → TCP → speaker round-trip without any cloud calls.

```powershell
$env:LOOPBACK = "1"
cargo run --release
```

### OpenAI (Whisper + Chat + TTS)

Set your key and run:

```powershell
setx OPENAI_API_KEY "sk-..."   # once; reopen the terminal afterwards
cargo run --release
```

Pipeline: OpenAI **Whisper** (speech-to-text) → **Chat Completions** (reply) →
**TTS** (24 kHz PCM, resampled to 16 kHz for the device).

## Configuration (environment variables)

| Variable         | Default            | Purpose                                   |
|------------------|--------------------|-------------------------------------------|
| `MODE`           | `windows`          | `windows` \| `openai` \| `loopback`       |
| `LOOPBACK`       | unset              | If set, forces loopback mode              |
| `OPENAI_API_KEY` | —                  | Required for `MODE=openai`                |
| `PORT`           | `9000`             | TCP listen port                           |
| `STT_MODEL`      | `whisper-1`        | Transcription model                       |
| `CHAT_MODEL`     | `gpt-4o-mini`      | Reply model                               |
| `TTS_MODEL`      | `gpt-4o-mini-tts`  | Speech model                              |
| `TTS_VOICE`      | `alloy`            | TTS voice                                 |

## Running

```powershell
cargo build --release
$env:LOOPBACK = "1"; .\target\release\server_voice_s3r.exe
```

The device must be on the **same LAN** and provisioned with this PC's IP and port
(e.g. `192.168.8.100:9000`). On Windows, allow the app through the firewall
(Private networks) the first time, or pre-create the rule:

```powershell
New-NetFirewallRule -DisplayName "VoiceS3R 9000" -Direction Inbound -LocalPort 9000 -Protocol TCP -Action Allow
```

### Example log

```
[  0.00s] listening on 0.0.0.0:9000
[  0.00s] waiting for the ATOM VoiceS3R to connect (hold its button to talk)...
[ 12.34s] ── connection from 192.168.8.132:62600 ──
[ 12.78s] [recv] 96000 bytes (~3.0s of 16kHz mono)
[ 13.91s] [stt] "what's the weather like"
[ 14.65s] [llm] "I can't check live weather, but I can help with..."
[ 15.80s] [tts] 144000 bytes @24k -> 96000 bytes @16k
[ 15.85s] [done] total 3.51s
```

## Roadmap

- Better local STT (whisper.cpp / `faster-whisper`) to replace the modest Windows
  recognizer — see the companion experiment in `rust_wthisper`.
- Streaming/partial responses to cut the round-trip latency.

## License

MIT
