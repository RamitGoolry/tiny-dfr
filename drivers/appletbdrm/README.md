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

Swapping in the patched module at runtime: stop tiny-dfr, unbind/unload the stock
`appletbdrm`, load this one, then restart tiny-dfr.

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
handshake but move it **off the commit's critical path** (Option B): a
non-blocking commit + a worker doing the complete send+ack, optionally coalescing
to the latest frame. That's real kernel-threading work, **deferred** until a
slider actually needs it.

## Patches vs upstream
(none — unmodified baseline. Patch A was tried and reverted; see above.)
