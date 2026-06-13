"""Local Whisper STT microservice for ServerVoiceS3R.

Loads an openai-whisper model ONCE and serves transcription over HTTP so the
Rust server doesn't pay the model-load cost on every utterance.

POST raw 16 kHz / mono / 16-bit-LE PCM to  http://127.0.0.1:9100/stt
  -> {"text": "..."}

Run with the venv that has `whisper` + numpy installed, e.g.:
  C:\\Users\\gravi\\Documents\\rust\\rust_wthisper\\.venv\\Scripts\\python.exe stt_server.py base.en
Model arg is optional (default base.en). Bigger = more accurate, slower:
  tiny.en < base.en < small.en < medium.en
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np
import whisper

# "turbo" (large-v3-turbo) is multilingual — auto-detects Russian/English/etc.
MODEL_NAME = sys.argv[1] if len(sys.argv) > 1 else "turbo"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 9100

print(f"[stt] loading whisper model '{MODEL_NAME}' (first run downloads it)...", flush=True)
model = whisper.load_model(MODEL_NAME)
print(f"[stt] model ready; listening on 127.0.0.1:{PORT}/stt", flush=True)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass  # quiet

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        data = self.rfile.read(n)
        text = ""
        try:
            # Raw s16le mono PCM -> float32 [-1, 1], which whisper accepts directly
            # (avoids needing ffmpeg to decode a file).
            audio = np.frombuffer(data, np.int16).astype(np.float32) / 32768.0
            # No fixed language -> Whisper auto-detects (Russian, English, ...).
            result = model.transcribe(audio, fp16=False)
            text = (result.get("text") or "").strip()
            lang = result.get("language", "?")
            print(f"[stt] {len(audio)/16000:.1f}s [{lang}] -> {text!r}", flush=True)
        except Exception as e:  # noqa: BLE001
            print(f"[stt] error: {e}", flush=True)
        body = json.dumps({"text": text}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
