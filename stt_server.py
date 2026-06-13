"""Local STT microservice for ServerVoiceS3R using faster-whisper (CTranslate2).

Much faster than openai-whisper on CPU (int8). Loads the model ONCE and serves
transcription over HTTP so the Rust server doesn't pay the load cost each time.

POST raw 16 kHz / mono / 16-bit-LE PCM to  http://127.0.0.1:9100/stt
  -> {"text": "...", "lang": "ru"|"en"|...}

Run with the venv that has faster-whisper + numpy:
  ...\\.venv\\Scripts\\python.exe stt_server.py small
Model arg optional (default small). Multilingual (Russian/English/...):
  tiny < base < small < medium < large-v3 < large-v3-turbo  (bigger = slower, more accurate)
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np
from faster_whisper import WhisperModel

MODEL_NAME = sys.argv[1] if len(sys.argv) > 1 else "small"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 9100

print(f"[stt] loading faster-whisper '{MODEL_NAME}' (int8/cpu; first run downloads it)...", flush=True)
model = WhisperModel(MODEL_NAME, device="cpu", compute_type="int8")
print(f"[stt] model ready; listening on 127.0.0.1:{PORT}/stt", flush=True)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass  # quiet

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        data = self.rfile.read(n)
        text, lang = "", "?"
        try:
            audio = np.frombuffer(data, np.int16).astype(np.float32) / 32768.0
            # beam_size=1 (greedy) + VAD = fast; language auto-detected.
            segments, info = model.transcribe(audio, beam_size=1, vad_filter=True)
            text = "".join(s.text for s in segments).strip()
            lang = info.language
            print(f"[stt] {len(audio)/16000:.1f}s [{lang}] -> {text!r}", flush=True)
        except Exception as e:  # noqa: BLE001
            print(f"[stt] error: {e}", flush=True)
        body = json.dumps({"text": text, "lang": lang}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
