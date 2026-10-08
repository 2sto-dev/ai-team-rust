---
name: Speech and telephony audio
description: Measurable STT/TTS, latency and barge-in criteria for voice systems over the phone.
---

- Test on telephone audio, not studio audio: 8 kHz narrowband (G.711 a-law/u-law), plus
  realistic noise, packet loss and jitter. Keep the test set fixed and versioned.
- STT: WER and CER per condition, plus accuracy on the words that matter (names, addresses,
  numbers, dates). Romanian diacritics and number formats are normalized before scoring.
- TTS: intelligibility on the phone channel, pronunciation of names and numbers, and MOS-style
  listening scores with the number of listeners stated.
- Latency end to end, from the caller's end of speech to the first audio of the reply, as p50
  and p95; streaming STT and TTS where the budget is tight.
- Turn detection and barge-in: false cut-ins, missed turns, and time to stop playback when the
  caller speaks.
- Log per call: timings per stage, transcript confidence and the decision taken, without
  storing more personal audio than the task allows.
