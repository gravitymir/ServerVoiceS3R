"""Diagnose the device's true sample rate from a captured utterance.

Reads debug_last.pcm (assumed s16le mono), and for each candidate SOURCE rate,
resamples to 16 kHz and asks the running STT service to transcribe it. The rate
that yields clean text is the device's real rate.
"""
import json
import urllib.request

import numpy as np

raw = np.fromfile("debug_last.pcm", dtype=np.int16)
print(f"samples={len(raw)}  max|amp|={int(np.abs(raw).max())}/32767  dur@16k={len(raw)/16000:.2f}s")
print()


def resample(x, src, dst):
    if src == dst:
        return x.astype(np.int16)
    n = int(len(x) * dst / src)
    xi = np.linspace(0, len(x) - 1, n)
    return np.interp(xi, np.arange(len(x)), x).astype(np.int16)


def stt(pcm_bytes):
    req = urllib.request.Request(
        "http://127.0.0.1:9100/stt", data=pcm_bytes, method="POST"
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.loads(r.read().decode())["text"]


for src in [8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000]:
    rs = resample(raw, src, 16000)
    try:
        text = stt(rs.tobytes())
    except Exception as e:  # noqa: BLE001
        text = f"<error {e}>"
    print(f"if device is {src:6d} Hz -> {text!r}")
