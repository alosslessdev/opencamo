# opencamo wire protocol v1

The phone (Android) runs a small HTTP server on the LAN. The desktop connects to it.
Transport: HTTP/1.1 over TCP, default port **47777**. All JSON is UTF-8.

Why MJPEG: every frame is an independent JPEG — trivially decodable on the
desktop (`image` crate), no codec licensing, sub-frame latency, and it can be
written straight into a v4l2loopback device configured for MJPEG. H.264 is a
possible v2 addition (`/v1/stream?format=h264`), not required for v1.

## Discovery (optional, convenience)

Phone advertises mDNS/Bonjour service `_opencamo._tcp.local` with TXT records:
- `ver=1`
- `name=<human readable device name>`

Manual IP entry is always supported; mDNS is not required for v1 clients.

## Endpoints

### `GET /v1/capabilities`

Response `200`, `Content-Type: application/json`:

```json
{
  "protocol": 1,
  "app": "opencamo",
  "version": "0.1.0",
  "device_name": "Pixel 7",
  "cameras": [
    {
      "id": "back",
      "label": "Back camera",
      "resolutions": [
        { "width": 1280, "height": 720, "fps": [30] },
        { "width": 1920, "height": 1080, "fps": [30] }
      ]
    },
    {
      "id": "front",
      "label": "Front camera",
      "resolutions": [
        { "width": 640, "height": 480, "fps": [30] }
      ]
    }
  ]
}
```

Rules:
- `fps` lists the discrete frame rates the phone can deliver for that
  resolution. The phone MUST re-query its camera list on capabilities request
  so hot-plugged/switched cameras appear.
- Resolution list MUST come from the camera's
  `StreamConfigurationMap.getOutputSizes(SurfaceHolder::class.java)` (or
  CameraX `Camera2CameraInfo` supported resolutions), not hardcoded.

### `GET /v1/stream`

Query parameters:

| param     | required | default | notes |
|-----------|----------|---------|-------|
| `camera`  | no       | `back`  | camera `id` from capabilities |
| `width`   | no       | largest supported | must be a supported size, else `400` |
| `height`  | no       | paired with width | |
| `fps`     | no       | 30      | clamped to a supported rate |

Response: `200`, `Content-Type: multipart/x-mixed-replace; boundary=frame`

Each part:

```
--frame\r\n
Content-Type: image/jpeg\r\n
Content-Length: <n>\r\n
\r\n
<n bytes of JPEG>\r\n
```

- The phone MUST drop frames rather than queue them (latest-frame-wins) so
  latency stays bounded.
- Changing `width`/`height`/`fps`/`camera` requires a new request; the phone
  closes the old stream when a new one is requested (single active stream).
- `400` with JSON `{"error": "..."}` for bad params, `404` for unknown camera.

### `GET /v1/ping`

Response: `200`, body `pong`. Used by the desktop to probe a candidate address.

## Errors

Non-2xx responses carry `Content-Type: application/json` body:
`{"error": "<message>"}`.

## Versioning

Breaking changes bump `protocol` in `/v1/capabilities` and add `/v2/...`
endpoints. v1 clients MUST check `"protocol": 1`.
