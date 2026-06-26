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
device `UPDATE_COMPLETE` ack (`appletbdrm_read_response`, 1000 ms timeout). A slow
ack stalls the whole pipe for up to ~1 s, which shows up as a Touch Bar button's
highlight "sticking" — and would make a continuously-updating slider hitch. The
fix work targets that ack/flush path (bounded timeout + skip-to-latest frame).

## Patches vs upstream
(none yet — this is the unmodified vendored baseline)
