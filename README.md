# tiny-dfr
The most basic dynamic function row daemon possible


## Dependencies
cairo, libinput, freetype, fontconfig, librsvg 2.59 or later, uinput enabled in kernel config

## Running as a user-session service

This fork runs tiny-dfr **in your graphical session** (not as a root system
service). That lets it drive PipeWire volume directly (via `wpctl` — no helper)
and read your compositor's focused-window events for app-aware layers, all in one
process. The cost is that the daemon needs device access as your user:

- **Touch Bar DRM card** already works if you're in the `video` group.
- **Display + keyboard backlight** brightness are root-owned sysfs attributes; the
  udev rule below `chmod`s them to the `video` group so the brightness and
  keyboard-illumination sliders can write them.
- **`/dev/uinput`** (key emission) and the **Touch Bar digitizer** need the
  `input` group. (`uaccess` doesn't work — uinput isn't a seat device and the
  digitizer sits on its own seat.) Install [`udev/99-tiny-dfr.rules`](udev/99-tiny-dfr.rules)
  and join the group — `just setup-udev`, or by hand:
  ```
  sudo cp udev/99-tiny-dfr.rules /etc/udev/rules.d/
  sudo udevadm control --reload && sudo udevadm trigger --name-match=uinput
  sudo usermod -aG input "$USER"   # then log out/in (or reboot)
  ```
  Verify after re-login with `ls -l /dev/uinput` (`crw-rw---- root input`) and
  `id` (shows `input`).
- Volume needs `wpctl` (WirePlumber) on `PATH`.

Then disable any old **system** service and run it in your session — either the
shipped user unit:
```
sudo systemctl disable --now tiny-dfr            # remove the old system service
mkdir -p ~/.config/systemd/user
cp share/systemd/tiny-dfr.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tiny-dfr
```
or, more simply, straight from the compositor (inherits the session env, but no
auto-restart): `exec-once = /usr/bin/tiny-dfr`.

## App-aware layers

Map a focused-window class to a layer with an `[AppLayers]` table in
`/etc/tiny-dfr/config.toml` — when that app is focused, its layer takes precedence
over the base layer:

```toml
[AppLayers]
Spotify = "media"
firefox = "media"
```

The class is the Hyprland `class` (see `hyprctl activewindow`). Values are layer
names from the registry (today: `media`, `fkeys`). An unmapped app falls back to
the base layer.

## Browser tabs

tiny-dfr selects a browser backend from the focused Hyprland window class:

- Chromium/Chrome use the HTTP Chrome DevTools Protocol target endpoints.
- Zen/Firefox use WebDriver BiDi over WebSocket.

Both backends default to port `9222`. The built-in empty-workspace launcher starts
Zen with the required remote agent:

```sh
zen-browser --remote-debugging-port=9222
```

The debugging flag only takes effect when the browser process starts. If Zen is
already running without it, fully quit Zen before launching it from tiny-dfr (or
start it with the command above). tiny-dfr persists Zen's active BiDi session ID
under `$XDG_RUNTIME_DIR/tiny-dfr/` so service restarts reattach instead of trying
to create a second Firefox session. It also reads the active HTML media element
over BiDi when Zen omits duration or position from MPRIS. The launcher expects
the packaged executable `zen-browser` and icon
`/usr/share/icons/hicolor/64x64/apps/zen-browser.png`.

Custom layers can use `{ BrowserTabs = "active" }`; the old `ChromiumTabs` key is
still accepted as an alias.

## License

tiny-dfr is licensed under the MIT license, as included in the [LICENSE](LICENSE) file.

* Copyright The Asahi Linux Contributors

Please see the Git history for authorship information.

tiny-dfr embeds Google's [material-design-icons](https://github.com/google/material-design-icons)
which are licensed under [Apache License Version 2.0](LICENSE.material)
Some icons are derivatives of material-icons, with edits made by kekrby.
