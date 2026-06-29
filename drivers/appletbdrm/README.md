# appletbdrm (T1-patched, vendored)

A vendored fork of the mainline Linux **`appletbdrm`** DRM driver
(`drivers/gpu/drm/tiny/appletbdrm.c`) — the driver for the 2016–2017 MacBook Pro
(T1) Touch Bar OLED, which is a small USB-connected display.

**Why it lives in this repo:** tiny-dfr (the userspace daemon) and this driver
are the two halves of the T1 Touch Bar stack on this machine. Vendoring the
driver fork next to the daemon keeps them versioned together — a daemon change
and the driver change it depends on can land in the same commit.

## Licensing
This directory is **GPL-2.0** — it is Linux kernel code (see the SPDX header in
`appletbdrm.c`, © 2023 Kerem Karabay). The rest of tiny-dfr is MIT/Apache-2.0.
They are separate works that communicate over DRM ioctls (no linking), so the
licenses don't conflict. **Keep the GPL-2.0 SPDX header intact.**

## Upstream baseline
Forked from `torvalds/linux` tag **v7.0**, `drivers/gpu/drm/tiny/appletbdrm.c`.
Re-sync by diffing against that path in a newer kernel tag.

## Build / install
```sh
make                  # build appletbdrm.ko against the running kernel headers
sudo make install     # install into the kernel's extramodules + depmod
```
For persistence across kernel updates, package via DKMS (copy this directory to
`/usr/src/appletbdrm-tinydfr-0.1/`, then `sudo dkms add/build/install`).

Hot-reload at runtime without a reboot: `make hotswap`. NOTE: the iBridge only
exposes the appletbdrm interface after `/usr/local/bin/touchbar-appletbdrm.sh`
switches it to USB config 2 and teaches the driver the T1 id via `new_id` — and
that `new_id` is lost on `rmmod`, so the hotswap target re-runs that script after
insmod (a bare rmmod/insmod leaves the device unbound at `05ac:8600`).

## Why we forked it
The stock driver flushes each frame **synchronously** and waits for a per-frame
device `UPDATE_COMPLETE` ack (`appletbdrm_read_response`, 1000 ms timeout) — and
this runs inside a *blocking* atomic commit, so the wait sits on the critical path
of userspace's DIRTYFB ioctl. A slow ack stalls the compositor's render loop for
up to ~1 s, which shows up as a Touch Bar button highlight "sticking" and would
make a continuously-updating slider hitch.

## What we tried, and what we learned
**Patch A — bounded ack timeout (100 ms) + stale-ack drain — TRIED AND REVERTED.**
Capping the ack wait did remove the 1 s freeze, but it broke the device: the
active (pressed) highlight stopped drawing *and* touch presses started dropping.
The iBridge is a **single USB device that serves both the display (this driver)
and the touch surface**, and abandoning/draining the appletbdrm ack mid-protocol
desyncs the firmware enough to disturb both halves.

**Lesson: the per-frame ack is load-bearing across the whole device — it cannot
be shortened or abandoned.** The only viable direction is to keep the *full*
handshake but move it **off the commit's critical path** (Option B).

**Patch B — async flush worker + pacing + ack drain — IMPLEMENTED, working.**
The atomic update now only copies the damaged pixels into a driver-owned
full-frame shadow buffer (BGR888), unions the dirty rect, and hands the slow USB
send + `UPDATE_COMPLETE` ack to a workqueue. `DIRTYFB` returns in ~ms so
tiny-dfr's loop never blocks → brightness/responsiveness stay smooth. The full
ack is kept (no Patch-A desync). Three pieces, each learned the hard way:

1. **Pacing** (`delayed_work`, one coalesced frame per run, ~30 fps). The stock
   driver was paced by the render loop; firing frames back-to-back overruns the
   device's request/response pipe (`-110` send timeouts).
2. **Wide ack timeout** (3 s in the worker) so a near-miss ack doesn't time out
   and skip a response.
3. **Ack-backlog drain** — the device occasionally emits a garbage short ack,
   leaving its real ack unread; the offset accumulates until the pipe chokes
   (garbage acks → `-110` send timeouts → frozen bar). After each frame, if we've
   fallen behind (`off`) or the read errored, drain the already-queued stale acks
   so the worker stays current. This is *not* Patch A: the load-bearing wait for
   each frame's own ack is untouched — we only mop up the stale tail behind it.

Diagnosis tooling: the `flush_log` module param
(`/sys/module/appletbdrm/parameters/flush_log`, off by default) logs per-frame
timing + the ack offset.

*Known residual:* the device emits a junk 16-byte message before each real ack,
so the worker's primary read trips on it and the drain mops up the real one —
costing the drain's empty-wait (~50 ms) per frame (~20 fps). Skipping those short
messages in `appletbdrm_read_response` (like the readiness-signal retry) is a
pending optimization.

## Patches vs upstream
**Patch B (async flush worker + pacing + drain)** — see above. Touches the device
struct (shadow buffer, accumulated damage rect, `flush_work`, `flush_seq`, lock),
`appletbdrm_flush_damage` (sync shadow-copy + queue), new `appletbdrm_flush_work`
(send+ack+drain off the critical path), `appletbdrm_drain_responses`, trimmed
`.atomic_check`, `DRM_GEM_SHADOW_PLANE_FUNCS` for plane state, worker drain +
buffer-free in `.atomic_disable`, and setup in `appletbdrm_probe`.

(Patch A was tried and reverted; see "What we tried".)
