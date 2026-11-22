#!/usr/bin/env python3
"""voice_call.py — prototyp Fazy 6: bot odbiera dzwonienia Matrix (m.call, MSC3245)
i prowadzi rozmowę głosową: audio z call -> STT (Mistral) -> vibe (sesja --resume)
-> TTS (Mistral) -> audio do call. Docelowo przepisane na Rust.

Env (jak bridge): MATRIX_HOMESERVER, MATRIX_ACCESS_TOKEN, MATRIX_BOT_USER_ID,
MATRIX_ROOM_ID, MATRIX_ALLOWED_USERS, MISTRAL_API_KEY, STATE_DIR (default ~/.local/share/vibe-matrix-rs)

Uruchomienie: python3 voice_call.py   (obok bridge'a; oba mogą działać razem —
call events idą przez to-device, więc nie kolidują z m.room.message)
"""
import asyncio
import json
import logging
import os
import struct
import time
import wave
from pathlib import Path

import aiohttp
import av
from aiortc import RTCPeerConnection, RTCSessionDescription, MediaStreamTrack
from aiortc.contrib.media import MediaBlackhole  # noqa: F401 (debug)

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("voice-call")

HS = os.environ.get("MATRIX_HOMESERVER", "https://matrix.owl-post.eu").rstrip("/")
TOKEN = os.environ.get("MATRIX_BOT_TOKEN", "")
BOT = os.environ.get("MATRIX_BOT_USER_ID", "@vibebot:matrix.owl-post.eu")
ROOM = os.environ.get("VIBE_ROOM_ID", "")
ALLOWED = {s.strip() for s in os.environ.get("MATRIX_ALLOWED_USERS", "@stanislaw:matrix.owl-post.eu").split(",") if s.strip()}
MISTRAL_KEY = os.environ.get("MISTRAL_API_KEY", "")
STATE_DIR = Path(os.environ.get("STATE_DIR", Path.home() / ".local/share/vibe-matrix-rs"))
STATE_DIR.mkdir(parents=True, exist_ok=True)
SESSION_FILE = STATE_DIR / "call-vibe-session.txt"

CALL_LIFETIME_MS = 60_000
STT_CHUNK_SEC = 3.5          # co ile sekund transkrypcja
MIN_AUDIO_SEC = 1.2          # minimum audio żeby STT miało sens
SAMPLE_IN = 48000            # WebRTC audio
SAMPLE_STT = 16000           # STT


def enc(s: str) -> str:
    from urllib.parse import quote
    return quote(s, safe="")


async def http_json(session, method, url, **kw):
    headers = kw.pop("headers", {})
    headers["Authorization"] = f"Bearer {TOKEN}"
    async with session.request(method, url, headers=headers, **kw) as r:
        body = await r.text()
        if r.status >= 400:
            raise RuntimeError(f"{method} {url} -> {r.status}: {body[:300]}")
        return json.loads(body) if body else {}


async def send_to_device(session, ev_type: str, content: dict, txn: str):
    url = f"{HS}/_matrix/client/v3/sendToDevice/{enc(ev_type)}/{enc(txn)}"
    payload = {"messages": {BOT: {"*": content}}}
    # sendToDevice target: user -> devices "*"
    await http_json(session, "PUT", url, json=payload)


async def stt(session, wav_bytes: bytes) -> str:
    if not MISTRAL_KEY:
        return ""
    form = aiohttp.FormData()
    form.add_field("model", "voxtral-mini-latest")
    form.add_field("file", wav_bytes, filename="chunk.wav", content_type="audio/wav")
    async with session.post(
        "https://api.mistral.ai/v1/audio/transcriptions",
        data=form,
        headers={"Authorization": f"Bearer {MISTRAL_KEY}"},
    ) as r:
        v = await r.json()
        if r.status != 200:
            log.warning("STT %s: %s", r.status, str(v)[:200])
            return ""
        return v.get("text", "").strip()


async def tts(session, text: str) -> bytes:
    """Zwraca PCM 48kHz stereo s16 (do puszczenia w call)."""
    if not MISTRAL_KEY or not text.strip():
        return b""
    body = {"model": "voxtral-mini-tts-latest", "input": text, "voice": "gb_jane_neutral", "response_format": "opus"}
    async with session.post(
        "https://api.mistral.ai/v1/audio/speech",
        json=body,
        headers={"Authorization": f"Bearer {MISTRAL_KEY}"},
    ) as r:
        v = await r.json()
        if r.status != 200 or "audio_data" not in v:
            log.warning("TTS %s: %s", r.status, str(v)[:200])
            return b""
        import base64
        opus = base64.b64decode(v["audio_data"])
    # dekoduj opus -> pcm 48k stereo przez av
    container = av.open(file=__import__("io").BytesIO(opus), format="ogg")
    resampler = av.AudioResampler(format="s16", layout="stereo", rate=SAMPLE_IN)
    out = bytearray()
    for frame in container.decode(audio=0):
        for rf in resampler.resample(frame):
            out += rf.to_ndarray().tobytes()
    if resampler:
        for rf in resampler.resample(None):
            out += rf.to_ndarray().tobytes()
    return bytes(out)


def pcm16_mono_to_wav(pcm: bytes, rate: int = SAMPLE_STT) -> bytes:
    import io
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm)
    return buf.getvalue()


async def run_vibe(prompt: str) -> str:
    """vibe -p z kontynuacją sesji (--resume), jak w Rust v3."""
    sid = SESSION_FILE.read_text().strip() if SESSION_FILE.exists() else None
    cmd = ["vibe", "-p", prompt, "--yolo", "--output", "json"]
    if sid:
        cmd += ["--resume", sid]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd, cwd=str(STATE_DIR / "vibe-work"),
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            env={**os.environ, "TERM": "dumb"},
        )
        out, err = await asyncio.wait_for(proc.communicate(), timeout=600)
        if proc.returncode != 0:
            log.warning("vibe rc=%s: %s", proc.returncode, err.decode()[:300])
            return ""
        try:
            events = json.loads(out.decode())
            new_sid = next((e.get("sessionId") for e in events if e.get("sessionId")), "")
            if new_sid:
                SESSION_FILE.write_text(new_sid + "\n")
            texts = [
                "".join(c.get("text", "") for c in e.get("content", []) if c.get("type") == "text")
                for e in reversed(events) if e.get("role") == "assistant"
            ]
            return next((t for t in texts if t.strip()), "")
        except Exception:
            return out.decode(errors="replace").strip()
    except asyncio.TimeoutError:
        return "(vibe timeout)"
    except Exception as e:
        return f"(vibe błąd: {e})"


class QueueAudioTrack(MediaStreamTrack):
    """Audio track do call: czyta PCM 48k stereo s16 z kolejki."""
    kind = "audio"

    def __init__(self):
        super().__init__()
        self._q: asyncio.Queue[bytes] = asyncio.Queue()
        self._pts = 0
        self._ts = 0.0

    def push_pcm48_stereo(self, pcm: bytes):
        self._q.put_nowait(pcm)

    async def recv(self):
        pcm = await self._q.get()
        samples = len(pcm) // 4  # stereo s16 = 4 bajty/sample
        import numpy as np
        arr = np.frombuffer(pcm, dtype=np.int16).reshape(-1, 2)
        frame = av.AudioFrame.from_ndarray(arr, format="s16", layout="stereo")
        frame.sample_rate = SAMPLE_IN
        frame.pts = self._pts
        frame.time_base = __import__("fractions").Fraction(1, SAMPLE_IN)
        self._pts += samples
        return frame


async def handle_call(session_http, call_id: str, sender: str, offer_sdp: str):
    log.info("📞 call %s od %s — answeruję", call_id[:12], sender)
    pc = RTCPeerConnection()
    out_track = QueueAudioTrack()
    pc.addTrack(out_track)

    in_pcm = bytearray()
    resampler = av.AudioResampler(format="s16", layout="mono", rate=SAMPLE_STT)

    @pc.on("track")
    def _on_track(track):
        if track.kind != "audio":
            return
        async def consume():
            nonlocal in_pcm
            try:
                while True:
                    frame = await track.recv()
                    for rf in resampler.resample(frame):
                        in_pcm += rf.to_ndarray().tobytes()
            except Exception:
                pass
        asyncio.ensure_future(consume())

    try:
        await pc.setRemoteDescription(RTCSessionDescription(sdp=offer_sdp, type="offer"))
        answer = await pc.createAnswer()
        await pc.setLocalDescription(answer)
        # poczekaj na ICE gathering (żeby SDP miało candidates)
        for _ in range(50):
            if pc.iceGatheringState == "complete":
                break
            await asyncio.sleep(0.1)
        sdp = pc.localDescription.sdp
        await send_to_device(session_http, "m.call.answer", {
            "type": "answer", "sdp": sdp, "lifetime": CALL_LIFETIME_MS,
            "version": 0, "call_id": call_id,
        }, txn=f"ans{int(time.time()*1000)}")
        log.info("✅ m.call.answer wysłany (sdp %d znaków)", len(sdp))
    except Exception as e:
        log.exception("answer fail: %s", e)
        await pc.close()
        return

    # pętla rozmowy: chunk -> STT -> vibe -> TTS -> gramy
    try:
        while True:
            await asyncio.sleep(STT_CHUNK_SEC)
            if pc.connectionState in ("failed", "closed"):
                break
            if len(in_pcm) < SAMPLE_STT * 2 * MIN_AUDIO_SEC:
                continue
            chunk = bytes(in_pcm)
            in_pcm.clear()
            text = await stt(session_http, pcm16_mono_to_wav(chunk))
            if not text:
                continue
            log.info("🗣️ STT: %.120s", text)
            reply = await run_vibe(f"[telefon, rozmowa głosowa od {sender}]: {text}")
            if not reply:
                continue
            log.info("🤖 vibe: %.120s", reply)
            pcm = await tts(session_http, reply[:1500])
            if pcm:
                out_track.push_pcm48_stereo(pcm)
    except Exception:
        log.exception("call loop")
    finally:
        await pc.close()
        log.info("📴 call %s zakończony", call_id[:12])


async def main():
    if not MISTRAL_KEY:
        log.warning("Brak MISTRAL_API_KEY — STT/TTS wyłączone, tylko answer/ICE.")
    since = None
    timeout_ms = 30000
    active_calls: dict[str, asyncio.Task] = {}
    async with aiohttp.ClientSession() as s:
        # pierwszy sync: tylko nowe
        while True:
            try:
                url = f"{HS}/_matrix/client/v3/sync?timeout={timeout_ms}"
                if since:
                    url += f"&since={enc(since)}"
                data = await http_json(s, "GET", url)
                since = data.get("next_batch", since)
                for ev in data.get("to_device", {}).get("events", []):
                    et = ev.get("type")
                    content = ev.get("content", {})
                    sender = ev.get("sender", "")
                    call_id = content.get("call_id", "")
                    if ALLOWED and sender not in ALLOWED:
                        continue
                    if et == "m.call.invite" and content.get("room_id") == ROOM:
                        offer = content.get("offer", {}).get("sdp", "")
                        if call_id in active_calls and not active_calls[call_id].done():
                            continue
                        active_calls[call_id] = asyncio.ensure_future(
                            handle_call(s, call_id, sender, offer))
                    elif et in ("m.call.hangup", "m.call.reject") and call_id in active_calls:
                        t = active_calls.pop(call_id)
                        t.cancel()
                        log.info("📴 hangup %s", call_id[:12])
            except asyncio.CancelledError:
                raise
            except Exception as e:
                log.warning("sync error: %s — retry 5s", e)
                await asyncio.sleep(5)


if __name__ == "__main__":
    if not TOKEN or not ROOM:
        raise SystemExit("Ustaw MATRIX_BOT_TOKEN i VIBE_ROOM_ID (env)")
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
