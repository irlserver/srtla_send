## Installation

Download the `.deb` for your device: `arm64` for ARM boards (Jetson, Orange Pi, Raspberry Pi and similar), `amd64` for x86 machines. Then install it:

```
sudo dpkg -i srtla_*_arm64.deb
```

The package installs `/usr/bin/srtla_send`. It runs on any Linux distribution with glibc 2.27 or newer, which includes Ubuntu 18.04 and Debian 10 and later.

Verify the download against `sha256sums.txt`:

```
sha256sum --check --ignore-missing sha256sums.txt
```

For arguments and configuration, see the [README](https://github.com/irlserver/srtla_send#usage).

---

