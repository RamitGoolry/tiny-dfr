# Out-of-tree build of the (T1-patched) appletbdrm Touch Bar DRM driver.
# `make` builds appletbdrm.ko against the running kernel's headers.
obj-m += appletbdrm.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(CURDIR)
MOD  := appletbdrm

.PHONY: all clean install hotswap revert

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

# Install into the running kernel's extramodules tree + refresh moddep.
install: all
	$(MAKE) -C $(KDIR) M=$(PWD) INSTALL_MOD_DIR=updates modules_install
	depmod -a

# Hot-reload the freshly-built module without a reboot. The iBridge only exposes
# the appletbdrm interface after touchbar-appletbdrm.sh switches it to USB config 2
# AND teaches appletbdrm the T1 id via `new_id` — and that new_id is lost on rmmod.
# So: stop the daemon, swap the module, re-run the switch script (re-adds new_id +
# binds; re-switches config if rmmod reverted it), wait for the device node, then
# restart the daemon (whose BindsTo dependency is that node). Needs root.
hotswap: all
	sudo systemctl stop tiny-dfr
	if lsmod | grep -qw $(MOD); then sudo rmmod $(MOD); fi
	sudo insmod $(PWD)/$(MOD).ko
	sudo /usr/local/bin/touchbar-appletbdrm.sh
	sudo udevadm settle --timeout=15
	sudo sh -c 'for n in $$(seq 50); do [ -e /dev/tiny_dfr_display ] && break; sleep 0.1; done'
	sudo systemctl start tiny-dfr
	@echo ">> hotswapped $(MOD); card2=$$(ls /dev/dri/ | grep -c card2), follow: journalctl -u tiny-dfr -f -o cat"

# Return to the stock in-tree module (unload this build, modprobe the original).
revert:
	sudo systemctl stop tiny-dfr
	if lsmod | grep -qw $(MOD); then sudo rmmod $(MOD); fi
	sudo modprobe $(MOD)
	sudo systemctl start tiny-dfr
	@echo ">> reverted to stock $(MOD)"
