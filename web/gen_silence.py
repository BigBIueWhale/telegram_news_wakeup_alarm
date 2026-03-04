#!/usr/bin/env python3
"""Generate a 1-second silent MP3 for audio session keep-alive."""
import subprocess, wave, tempfile, os

dur_s = 1
rate = 44100
samples = dur_s * rate

# Write silent WAV (all-zero samples) via stdlib
wav_path = tempfile.mktemp(suffix=".wav")
with wave.open(wav_path, "w") as w:
    w.setnchannels(1)
    w.setsampwidth(2)  # 16-bit
    w.setframerate(rate)
    w.writeframes(b"\x00\x00" * samples)

# Encode to MP3
mp3_path = os.path.join(os.path.dirname(__file__), "silence.mp3")
subprocess.run(
    ["ffmpeg", "-y", "-i", wav_path, "-codec:a", "libmp3lame", "-b:a", "32k", mp3_path],
    check=True,
    capture_output=True,
)
os.unlink(wav_path)

sz = os.path.getsize(mp3_path)
print(f"Created {mp3_path} ({sz} bytes, {dur_s}s silent mono 32kbps)")
