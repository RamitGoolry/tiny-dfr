# tiny-dfr
The most basic dynamic function row daemon possible


## Dependencies
cairo, libinput, freetype, fontconfig, librsvg 2.59 or later, uinput enabled in kernel config

## Volume bridge (optional, for the volume slider)

The volume slider drives **PipeWire** so it matches the same sink the media keys
and any on-screen display use. The daemon itself can't do this: it runs as a
system service and drops to `nobody`, which has no access to the user's
per-session PipeWire socket. So volume goes through a small session-side helper.

How it works:

- The daemon writes the desired level to `/run/tiny-dfr/volume.target` and reads
  the live level from `/run/tiny-dfr/volume.current` (created mode 0666 as root
  before the privilege drop).
- [`helpers/tiny-dfr-volume`](helpers/tiny-dfr-volume), running **in your
  session**, applies the target with `wpctl set-volume` and republishes the live
  volume on every change. It needs `wpctl` (WirePlumber) and `pactl` (PipeWire's
  PulseAudio shim).

Setup (three pieces, all outside the daemon binary):

1. **Make `/run/tiny-dfr` writable.** The shipped unit uses `ProtectSystem=strict`,
   so grant it a runtime dir via a drop-in:
   ```
   sudo install -d /etc/systemd/system/tiny-dfr.service.d
   printf '[Service]\nRuntimeDirectory=tiny-dfr\n' \
     | sudo tee /etc/systemd/system/tiny-dfr.service.d/runtime-dir.conf
   sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr
   ```
2. **Put the helper on `PATH`** (symlink so it tracks this repo):
   ```
   ln -sf "$PWD/helpers/tiny-dfr-volume" ~/.local/bin/tiny-dfr-volume
   ```
3. **Launch it in your session.** For example, with Hyprland:
   ```
   exec-once = ~/.local/bin/tiny-dfr-volume
   ```

Without this helper the volume slider is inert (brightness and keyboard
illumination work without it — they write sysfs directly).

## License

tiny-dfr is licensed under the MIT license, as included in the [LICENSE](LICENSE) file.

* Copyright The Asahi Linux Contributors

Please see the Git history for authorship information.

tiny-dfr embeds Google's [material-design-icons](https://github.com/google/material-design-icons)
which are licensed under [Apache License Version 2.0](LICENSE.material)
Some icons are derivatives of material-icons, with edits made by kekrby.
