# Browser DOM-diff streaming

`tb-sandbox browser stream` is an efficient, interactive alternative to VNC for
seeing and driving the Chromium that runs inside a sandbox. Instead of encoding a
pixel framebuffer, it streams the **DOM and its mutations** out of the browser
and re-renders them on a client. Bandwidth tracks how much the page changes, not
how big the screen is, and — unlike `browser view` — it needs no display server
in the guest (no Xvfb/x11vnc), so it's also cheaper at VM density.

## Why DOM instead of pixels

VNC (`browser view`) forces headed mode, runs Xvfb + a window manager + x11vnc in
the guest, and ships a framebuffer whose cost scales with screen area and frame
rate regardless of what actually changed. On the file-relay forwarding fallback
it is barely interactive. A DOM-diff stream sends a one-time serialized DOM and
then small incremental events (node inserts/removes, attribute/text changes,
scroll, input) — typically orders of magnitude less data for normal web content.

## Building block: rrweb

We use [rrweb](https://github.com/rrweb-io/rrweb) (MIT), the mature DOM
record/replay library behind most session-replay products, in its **live mode**:

- **Record side** runs in Chromium. It takes one full snapshot (each node gets an
  integer id) and then a `MutationObserver` + input/scroll/etc. observers emit
  compact incremental events that reference nodes by id. rrweb inlines parsed
  stylesheets and rewrites relative URLs to absolute, so the client can render
  faithfully; `<script>` is neutralized (replay is passive DOM reconstruction).
- **Replay side** runs in the viewer: `new rrweb.Replayer([], {liveMode:true})`,
  `startLive(...)`, and `addEvent(ev)` for each event off the wire.

The vendored bundle is `assets/browser-stream/rrweb.min.js` (pinned, embedded in
the binary). Both record and replay use the same bundle.

## Architecture

```
viewer browser ──http──> [host stream server] ──CDP ws──> forwarded :9222 ──> guest Chromium
  rrweb.Replayer  <─ws──   src/browser_stream.rs   addBinding / Runtime.bindingCalled ┘
  input ──ws──>            Input.dispatch{Mouse,Key}Event / Page.navigate
```

Everything new is **host-side** (`src/browser_stream.rs` + the CLI command); the
guest image is unchanged. The existing `sdk.forward_port` gives the host a local
CDP endpoint (native/low-latency over gvproxy, file-relay otherwise).

### The CDP loopback relay
Modern Chromium (verified on 148) **ignores `--remote-debugging-address` and
binds the DevTools port to `127.0.0.1` only** for DNS-rebinding safety. The
gvproxy forward routes to the guest's *external* interface (`192.168.127.2`),
where nothing is listening — so a direct forward of `9222` is refused/reset.
(VNC doesn't hit this: x11vnc honours `-listen 0.0.0.0`.)

So `browser stream` launches a tiny **in-guest TCP relay** (`launch_cdp_relay` in
`src/cli.rs`, a detached python3 one-shot) that listens on `0.0.0.0:<cdp_port+1>`
and bridges to Chromium's `127.0.0.1:<cdp_port>`, then forwards the relay port.
This needs no image rebuild and no change to Chromium's flags; it relies only on
the guest's python3 (already required by the file-relay forwarder). The relay is
idempotent and dies with the VM.

### Capture path
1. `GET http://<cdp>/json` → pick the `page` target, take its
   `webSocketDebuggerUrl`, and **rewrite its host:port** to the forwarded
   endpoint (Chrome reports its own bind address — the classic gotcha).
2. Over the CDP WebSocket: `Page.enable`, `Runtime.enable`,
   `Runtime.addBinding {name:"__tbRrwebSend"}`,
   `Page.addScriptToEvaluateOnNewDocument {source}` (re-injects on every
   navigation), and a one-shot `Runtime.evaluate {source}` for the current page.
   `source` = the rrweb bundle + a bootstrap that calls
   `rrweb.record({ emit: e => __tbRrwebSend(JSON.stringify(e)) })`, guarded by a
   `window.__tbRrwebStarted` flag so the two injection paths don't double-start.
3. Each `Runtime.bindingCalled` for our binding carries one serialized rrweb
   event; it's broadcast to all connected viewers.

When a viewer connects, the server asks the live recorder for a fresh full
snapshot (`rrweb.record.takeFullSnapshot(true)`) so late joiners get a complete
tree rather than only subsequent diffs.

### Client-side feel: native hover, cursor, and scrolling
Because the client holds the full DOM, three things are done **locally** for zero
latency — the overlay is `pointer-events: none`, so the real pointer is "over"
the replayed content and the client browser renders them natively:

- **`:hover`** — CSS `:hover` is driven by the real pointer position, so it just
  works (you can't emulate it by dispatching synthetic events). JS-driven hover
  on the real page still needs the move forwarded (below).
- **CSS `cursor`** — the pointer shows the right cursor (hand over links, text
  over inputs) natively.
- **Scrolling** — the wheel scrolls the iframe natively (instant, including
  nested scroll regions); the viewer only syncs the resulting scroll *position*
  to the real browser (debounced) for lazy-load and click alignment.

Input is captured by listeners on the iframe document (re-wired on each full
snapshot) and forwarded; clicks are `preventDefault`'d so the replay never
navigates locally. Keyboard stays on the overlay, which is still focusable.

### Interaction path (back-channel)
The viewer forwards JSON input messages up the same WebSocket; the server
translates each into CDP:

| Viewer message (`t`) | CDP call |
| --- | --- |
| `mouse` (`a`: down/up/move) | `Input.dispatchMouseEvent` (mousePressed/Released/Moved) |
| `wheel` | `Input.dispatchMouseEvent` (mouseWheel) |
| `key` (`a`: down/up) | `Input.dispatchKeyEvent` (keyDown/keyUp, with `text` on down) |
| `nav` | `Page.navigate` |

Coordinates are page coordinates: the viewer divides client coordinates by the
fit-to-stage scale, so dispatch is coordinate-based (which also handles overlays
correctly). The translation is a pure function (`input_to_cdp`) and unit-tested.

### Transport
- One `TcpListener` on the host serves the viewer page (`/`), the rrweb bundle
  (`/rrweb.js`), and the WebSocket (`/ws`). HTTP is hand-rolled in the same
  minimal style as `src/preview.rs`; the WebSocket handshake is read by peeking
  (not consuming) the request so `tokio_tungstenite::accept_async` can complete
  it. CDP and the viewer are both plain `ws://` (localhost / in-VM), so no TLS
  stack is pulled in.

## Wire format

- **Server → viewer:** each WebSocket text message is one rrweb event (raw JSON).
  The viewer `JSON.parse`s it and calls `replayer.addEvent`.
- **Viewer → server:** `{"t":"mouse","a":"down","x":..,"y":..,"button":"left","modifiers":N}`,
  `{"t":"wheel","x":..,"y":..,"dx":..,"dy":..}`,
  `{"t":"key","a":"down","key":"a","code":"KeyA","keyCode":65,"text":"a","modifiers":N}`,
  `{"t":"nav","url":"https://…"}`. `modifiers` is the CDP bitmask
  (Alt=1, Ctrl=2, Meta=4, Shift=8).

## Lossy cases / limitations (current)

The DOM approach is faithful for HTML/CSS/text and inherently lossy for opaque
visual surfaces:

- **Canvas / WebGL** — not serialized (recording is off by default here).
- **`<video>` / media** — play/pause/seek state replays, but not the pixels.
- **Cross-origin iframes** — only the main frame is recorded for now.
- **External resources behind auth/CORS** — the client fetches images/fonts from
  their original URLs; assets it can't reach will not render. (Inside the sandbox
  we *could* proxy + inline these at egress — a future improvement.)

## Planned follow-ups

- **Pixel fallback** for canvas/WebGL/video: rrweb's canvas-WebRTC plugin or a
  region-cropped `Page.startScreencast` layered under the DOM stream.
- **Cross-origin iframes:** `Target.setAutoAttach` + per-frame injection +
  `recordCrossOriginIframes`.
- **Resource inlining at egress** for faithful auth/CORS-gated assets.
- **Parallel a11y-tree channel** (`Accessibility.getFullAXTree`) as a compact,
  token-efficient observation for an LLM, separate from the human view.
- **Hosted multi-tenant routing** through `src/preview.rs` / the API server
  instead of a direct host-localhost listener.

## Try it

```sh
# create a sandbox with gvproxy networking (default when gvproxy is installed)
tb-sandbox browser stream my-sandbox      # prints stream_url=http://127.0.0.1:PORT
# open the URL, type a site in the bar, click/type — it drives the real Chromium
```

Verify it actuates the real browser by diffing against `browser view` (VNC)
side-by-side, or `Runtime.evaluate` of `document.title`/`location.href` over CDP.
