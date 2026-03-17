Pre-built propolis-server with string I/O emulation patch for nested virtualization (Proxmox/QEMU).

- Upstream: [oxidecomputer/propolis@__SHORT__](https://github.com/oxidecomputer/propolis/commit/__SHORT__)
- Patch: [string I/O instruction emulation](https://github.com/swherdman/propolis/commit/c2bc58f9cbf8596dc82d6d9d8ce7b06bf9581260)

## Quick install
```bash
curl -fL https://github.com/__REPO__/releases/download/__TAG__/patch-propolis.sh | bash
```

## Manual instructions
### 1. Download the patched binary
```bash
curl -fL https://github.com/__REPO__/releases/download/__TAG__/propolis-server.gz | gunzip > /tmp/propolis-server
chmod +x /tmp/propolis-server
```

### 2. Swap into the tarball
```bash
cd /tmp && mkdir propolis-repack && cd propolis-repack
tar xzf ~/omicron/out/propolis-server.tar.gz
cp /tmp/propolis-server root/opt/oxide/propolis-server/bin/propolis-server
tar czf ~/omicron/out/propolis-server.tar.gz oxide.json root/
rm -rf /tmp/propolis-repack /tmp/propolis-server
```

### 3. Install
**Fresh deploy:**
```bash
cd ~/omicron && pfexec ./target/release/omicron-package install
```

**Existing deployment (zones already running):**
```bash
cd ~/omicron
pfexec ./target/release/omicron-package uninstall
pfexec ./target/release/omicron-package install
```
