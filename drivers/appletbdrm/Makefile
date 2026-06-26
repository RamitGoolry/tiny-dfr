# Out-of-tree build of the (T1-patched) appletbdrm Touch Bar DRM driver.
# `make` builds appletbdrm.ko against the running kernel's headers.
obj-m += appletbdrm.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(CURDIR)

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

# Install into the running kernel's extramodules tree + refresh moddep.
install: all
	$(MAKE) -C $(KDIR) M=$(PWD) INSTALL_MOD_DIR=updates modules_install
	depmod -a
