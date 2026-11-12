"""Push a soundtrack and its animation to the board as one streamed body.

`POST /av` takes records of `tag | u16le length | payload`, in the order they should
happen: `A` is PCM16LE mono at 24 kHz, `F` is a 128x32 1-bpp frame (512 bytes), `E`
ends the body. Playing is what clocks the stream — an `A` record blocks the board's
I2S write for exactly as long as its audio lasts, so a frame written in front of it is
on screen while that audio plays. Nothing is buffered on either end beyond a socket.

    python tools/av_push.py --audio clip.pcm --frames clip.1bpp --host 192.168.3.7
    python tools/av_push.py --audio clip.pcm --frames clip.1bpp --fps 12 --limit 5

`--audio` is raw PCM, mono, 16-bit, at `--rate` (default 24000) — the shape `sox` makes:

    sox in.mp3 -t raw -e signed -b 16 -c 1 -r 24000 clip.pcm

`--frames` is a strip of 512-byte 128x32 page-format frames, back to back, which is
what the bench probe's `media/frames.1bpp` is. `--fps` decides how many audio bytes go
with each frame, so a frame strip and an audio file of the same length stay in step
even when their lengths disagree slightly.

This script is a *host* tool: it lives with the firmware, but nothing on the board runs
it. It is the smallest thing that speaks the format, so the format is documented by
something that works rather than by a diagram.
"""

import argparse
import socket
import struct
import sys
import time

FRAME_BYTES = 512  # 128 x 32 / 8


def records(audio: bytes, frames: bytes, rate: int, fps: float):
    """Yield the body, one record at a time, frame first then that frame's audio."""
    per_frame_bytes = int(rate / fps) * 2
    if per_frame_bytes <= 0 or per_frame_bytes > 65535:
        raise SystemExit(f"--fps {fps} at {rate} Hz does not fit a u16 record: use 1..32767 fps")
    pos = 0
    for i in range(0, len(frames) - FRAME_BYTES + 1, FRAME_BYTES):
        yield b"F" + struct.pack("<H", FRAME_BYTES) + frames[i:i + FRAME_BYTES]
        if pos < len(audio):
            piece = audio[pos:pos + per_frame_bytes]
            # An odd piece would be half a sample; the board refuses that on purpose.
            if len(piece) % 2:
                piece = piece[:-1]
            pos += len(piece)
            yield b"A" + struct.pack("<H", len(piece)) + piece
    while pos < len(audio):
        piece = audio[pos:pos + 65534]
        if len(piece) % 2:
            piece = piece[:-1]
        pos += len(piece)
        yield b"A" + struct.pack("<H", len(piece)) + piece
    yield b"E" + struct.pack("<H", 0)


def main() -> int:
    ap = argparse.ArgumentParser(description="Play a PCM file and a frame strip on the board together")
    ap.add_argument("--audio", required=True, help="raw PCM16LE mono file")
    ap.add_argument("--frames", required=True, help="512-byte 128x32 1-bpp frames, back to back")
    ap.add_argument("--host", default="192.168.3.7", help="board address (default 192.168.3.7)")
    ap.add_argument("--port", type=int, default=80)
    ap.add_argument("--rate", type=int, default=24000, help="sample rate of --audio (default 24000)")
    ap.add_argument("--fps", type=float, default=12.0, help="frames per second (default 12)")
    ap.add_argument("--limit", type=float, default=0.0, help="stop after N seconds, for a look first")
    ap.add_argument("--pin", default="", help="X-Portal-Pin, if the board has one set")
    ap.add_argument("--dry-run", action="store_true", help="do not send; print the plan")
    args = ap.parse_args()

    audio = open(args.audio, "rb").read()
    frames = open(args.frames, "rb").read()
    if args.limit > 0:
        audio = audio[: int(args.limit * args.rate) * 2]
        frames = frames[: int(args.limit * args.fps) * FRAME_BYTES]
    seconds = len(audio) / 2 / args.rate
    nframes = len(frames) // FRAME_BYTES
    print(
        f"audio {len(audio)} B ({seconds:.2f} s @ {args.rate} Hz), "
        f"frames {nframes} ({nframes / args.fps:.2f} s @ {args.fps:g} fps)"
    )
    body = list(records(audio, frames, args.rate, args.fps))
    if args.dry_run:
        print(f"{len(body)} records, {sum(len(r) for r in body)} bytes of body")
        return 0

    try:
        s = socket.create_connection((args.host, args.port), timeout=10)
    except OSError as e:
        raise SystemExit(f"could not reach {args.host}:{args.port}: {e}")
    s.settimeout(120)
    head = (
        f"POST /av HTTP/1.1\r\nHost: {args.host}:{args.port}\r\n"
        "Content-Type: application/x-esp32-av\r\n"
        "Transfer-Encoding: chunked\r\nConnection: close\r\n"
    )
    if args.pin:
        head += f"X-Portal-Pin: {args.pin}\r\n"
    s.sendall(head.encode() + b"\r\n")

    started = time.time()
    sent = 0
    for rec in body:
        # One chunk per record, so the board's parser sees exactly the pieces a real
        # host would write and the socket's back-pressure is per record, not per song.
        s.sendall(f"{len(rec):x}\r\n".encode() + rec + b"\r\n")
        sent += len(rec)
    s.sendall(b"0\r\n\r\n")

    buf = bytearray()
    while True:
        try:
            piece = s.recv(4096)
        except socket.timeout:
            break
        if not piece:
            break
        buf += piece
    s.close()
    text = buf.decode("utf-8", "replace")
    print(text.split("\r\n\r\n", 1)[-1] if "\r\n\r\n" in text else text)
    print(f"wrote {sent} B in {time.time() - started:.2f} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
