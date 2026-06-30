# tiny-dfr dev/deploy tasks.  `just install` rebuilds and redeploys.

# Rebuild (release), install the binary, restart the running daemon.
install: build
    sudo install -Dm755 target/release/tiny-dfr /usr/bin/tiny-dfr
    systemctl --user restart tiny-dfr || echo "(no user service — relaunch tiny-dfr in your session)"

# Release build.
build:
    cargo build --release

# Rebuild + hotswap the appletbdrm driver into the running kernel (transient).
hotswap-driver:
    cd ./drivers/appletbdrm/ && make hotswap

# Install the patched driver permanently via DKMS (survives reboots + kernel
# upgrades). Removes the stale non-DKMS module in updates/ that would shadow it.
install-driver:
    sudo rm -rf /usr/src/appletbdrm-tinydfr-0.1
    sudo cp -r drivers/appletbdrm /usr/src/appletbdrm-tinydfr-0.1
    sudo dkms remove -m appletbdrm-tinydfr -v 0.1 --all 2>/dev/null || true
    sudo dkms add -m appletbdrm-tinydfr -v 0.1
    sudo dkms build -m appletbdrm-tinydfr -v 0.1
    sudo dkms install -m appletbdrm-tinydfr -v 0.1
    sudo rm -f /lib/modules/$(uname -r)/updates/appletbdrm.ko.zst
    sudo depmod -a
    @echo ">> DKMS-installed. Verify after reboot: modinfo appletbdrm | grep srcversion (want 70E77…)"

# First-time: grant /dev/uinput to the `input` group and add yourself to it.
# Log out and back in (or reboot) afterwards so the group membership applies.
setup-udev:
    sudo cp udev/99-tiny-dfr.rules /etc/udev/rules.d/
    sudo udevadm control --reload
    sudo udevadm trigger --name-match=uinput
    sudo udevadm trigger --action=add --subsystem-match=backlight --subsystem-match=leds
    sudo usermod -aG input "$USER"
    @echo "added $USER to 'input' — log out/in (or reboot) to apply (backlight works now after a daemon restart)"

# First-time (optional): install + enable the systemd user service.
setup-service:
    mkdir -p ~/.config/systemd/user
    cp share/systemd/tiny-dfr.service ~/.config/systemd/user/
    systemctl --user daemon-reload
    systemctl --user enable --now tiny-dfr
