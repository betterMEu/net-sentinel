"""Generate the original, royalty-free WAV melodies bundled with Net Sentinel."""

from __future__ import annotations

import math
import pathlib
import struct
import wave

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "assets" / "sounds"
SAMPLE_RATE = 44_100

NOTES = {
    "C4": 261.63,
    "D4": 293.66,
    "E4": 329.63,
    "F4": 349.23,
    "G4": 392.00,
    "A4": 440.00,
    "B4": 493.88,
    "C5": 523.25,
    "D5": 587.33,
    "E5": 659.25,
    "G5": 783.99,
}


def tone(frequency: float, seconds: float, gain: float = 0.34) -> list[float]:
    count = int(seconds * SAMPLE_RATE)
    attack = max(1, int(0.012 * SAMPLE_RATE))
    release = max(1, int(min(0.16, seconds * 0.45) * SAMPLE_RATE))
    result: list[float] = []
    for i in range(count):
        envelope = min(1.0, i / attack, (count - i) / release)
        phase = 2.0 * math.pi * frequency * i / SAMPLE_RATE
        # A soft bell-like additive timbre, synthesized specifically for this project.
        sample = (
            math.sin(phase)
            + 0.34 * math.sin(2.01 * phase) * math.exp(-3.0 * i / count)
            + 0.12 * math.sin(3.99 * phase) * math.exp(-5.0 * i / count)
        )
        result.append(sample * envelope * gain)
    return result


def render(filename: str, score: list[tuple[str | None, float]], tempo: float) -> None:
    samples: list[float] = []
    beat_seconds = 60.0 / tempo
    for name, beats in score:
        seconds = beats * beat_seconds
        if name is None:
            samples.extend([0.0] * int(seconds * SAMPLE_RATE))
        else:
            samples.extend(tone(NOTES[name], seconds))

    peak = max(max(abs(value) for value in samples), 1.0)
    pcm = bytearray()
    for value in samples:
        pcm += struct.pack("<h", int(max(-1.0, min(1.0, value / peak)) * 32767))

    OUT.mkdir(parents=True, exist_ok=True)
    with wave.open(str(OUT / filename), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(SAMPLE_RATE)
        wav.writeframes(pcm)


def main() -> None:
    render(
        "soft-chime.wav",
        [("E5", 0.75), (None, 0.08), ("C5", 1.15)],
        118,
    )
    render(
        "bright-bells.wav",
        [("C5", 0.35), ("E5", 0.35), ("G5", 0.75), (None, 0.08), ("E5", 0.65)],
        132,
    )
    render(
        "gentle-alert.wav",
        [("A4", 0.55), (None, 0.07), ("D5", 0.55), (None, 0.07), ("A4", 0.9)],
        105,
    )


if __name__ == "__main__":
    main()

