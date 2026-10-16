# opencamo

Open-source Camo Studio alternative for Linux: use an Android phone as a
wireless webcam.

```
┌─────────────┐   HTTP/MJPEG over LAN    ┌──────────────────────┐
│ Android app │ ───────────────────────► │ desktop app (Rust)   │
│ CameraX +   │  /v1/capabilities        │ - egui preview       │
│ MJPEG server│  /v1/stream (multipart)  │ - JPEG decode        │
└─────────────┘                          │ - writes /dev/videoN │
                                         │   (v4l2loopback)     │
                                         └──────────────────────┘
                                                    │
                                          any V4L2 app sees a
                                          normal webcam there
```

## Components

| dir         | what |
|-------------|------|
| `android/`  | Kotlin app: CameraX capture → MJPEG HTTP server on the phone (port 47777) |
| `desktop/`  | Rust/egui app: connects to the phone, previews the stream, feeds a v4l2loopback `/dev/videoN` device |
| `tools/`    | `mock_phone.py` — phone simulator for testing the desktop app without a phone |
| `PROTOCOL.md` | wire protocol both sides implement |

## Does this need DKMS?

The *project* does not need DKMS. Making the stream appear as a normal
`/dev/videoN` webcam does require the `v4l2loopback` kernel module, which on
every mainstream distro is installed **via DKMS** (`v4l2loopback-dkms` package)
so it survives kernel updates:

```sh
# Arch
sudo pacman -S v4l2loopback-dkms linux-headers
# Debian/Ubuntu
sudo apt install v4l2loopback-dkms
# Fedora
sudo dnf install akmod-v4l2loopback kernel-devel
sudo modprobe v4l2loopback devices=1 card_label=opencamo exclusive_caps=1
```

DKMS is just the packaging mechanism for that module — users install it once
and forget it. The desktop app also works without it: the egui preview shows
the phone camera directly, so the whole pipeline is verifiable with no kernel
module and no external apps. The loopback writer is an optional toggle.

Why not implement the webcam as a kernel module ourselves (UVC gadget /
custom chardev)? A custom `/dev/video` driver means signing, Secure Boot
friction, and per-kernel maintenance. Writing into the existing, widely
packaged v4l2loopback is the boring, compatible choice.

## Quick test (no phone needed)

```sh
pip install pillow
python3 tools/mock_phone.py &          # fake phone on :47777
cd desktop && cargo run --release -- --selftest http://127.0.0.1:47777 60
```

## Real usage

1. Install the `android/` app, grant camera permission, press Start.
2. On Linux: `cd desktop && cargo run --release`, enter the phone IP shown in
   the app, Connect, pick camera/resolution, preview.
3. Enable loopback (requires v4l2loopback loaded): pick `/dev/videoN` with
   card label `opencamo`. OBS, browsers, Zoom etc. now see it as a webcam.
