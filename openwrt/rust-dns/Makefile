# SPDX-License-Identifier: GPL-3.0-only
#
# rust-dns service package.
#
# The binary is built outside the OpenWrt buildroot (Rust 1.100.0, static musl,
# ring TLS provider) and published as
#     rust-dns-<version>-<arch>.tar.gz
# CI records the SHA256 in hashes/<arch>.sha256.  This Makefile only downloads,
# unpacks and installs the prebuilt binary.  It is shared by the opkg (23.05,
# 24.10) and apk (25.12/main) backends.

include $(TOPDIR)/rules.mk

PKG_NAME:=rust-dns
PKG_VERSION:=0.1.0
PKG_RELEASE:=2

PKG_MAINTAINER:=rust-dns contributors
PKG_LICENSE:=GPL-3.0-only

# Override RUST_DNS_REPO when the release repository differs.
RUST_DNS_REPO?=moliango/rust-dns
RUST_DNS_RELEASE_URL?=https://github.com/$(RUST_DNS_REPO)/releases/download/v$(PKG_VERSION)

# Map OpenWrt's ARCH (e.g. aarch64_cortex-a53, arm_cortex-a7_neon-vfpv4,
# x86_64) to the release artifact name.  v1 deliberately does not support
# MIPS/armv5 because Rust ships no prebuilt musl std for them.
ifneq ($(filter x86_64%,$(ARCH)),)
  RUST_DNS_ARCH?=x86_64
else ifneq ($(filter aarch64%,$(ARCH)),)
  RUST_DNS_ARCH?=aarch64
else ifneq ($(filter arm_cortex-a%,$(ARCH)),)
  # OpenWrt armv7 targets are named arm_cortex-a<variant>.
  RUST_DNS_ARCH?=armv7
else ifneq ($(filter arm_v7%,$(ARCH)),)
  RUST_DNS_ARCH?=armv7
else ifneq ($(filter arm%,$(ARCH)),)
  # armv5/armv6 targets must not receive the armv7 artifact.
  RUST_DNS_ARCH?=unsupported
  $(warning rust-dns: '$(ARCH)' is armv5/armv6; v1 only ships armv7 (arm_cortex-a*) binaries)
else
  # Do not abort feed metadata generation for unrelated targets; the package
  # simply has no artifact for this architecture.
  RUST_DNS_ARCH?=unsupported
  $(warning rust-dns: unsupported architecture '$(ARCH)'; v1 supports x86_64, aarch64 and armv7 only)
endif

RUST_DNS_HASH_FILE:=$(CURDIR)/hashes/$(RUST_DNS_ARCH).sha256
RUST_DNS_SHA256:=$(shell sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' $(RUST_DNS_HASH_FILE) 2>/dev/null | head -n1)

PKG_SOURCE:=rust-dns-$(PKG_VERSION)-$(RUST_DNS_ARCH).tar.gz
PKG_SOURCE_URL:=$(RUST_DNS_RELEASE_URL)
PKG_HASH:=$(RUST_DNS_SHA256)
PKG_BUILD_DIR:=$(BUILD_DIR)/rust-dns-$(PKG_VERSION)-$(RUST_DNS_ARCH)
PKG_BUILD_PARALLEL:=1
PKG_BUILD_DEPENDS:=

ifeq ($(strip $(PKG_HASH)),)
  $(warning rust-dns: no SHA256 found in $(RUST_DNS_HASH_FILE); the download will fail until CI fills it in)
else ifeq ($(RUST_DNS_SHA256),0000000000000000000000000000000000000000000000000000000000000000)
  $(warning rust-dns: $(RUST_DNS_HASH_FILE) still contains the zero placeholder; replace it with the real release digest before building)
endif

include $(INCLUDE_DIR)/package.mk

define Package/rust-dns
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=DNS
  TITLE:=A fast DNS forwarder with local/remote split and stats API
  URL:=https://github.com/$(RUST_DNS_REPO)
  DEPENDS:=+ucode +ucode-mod-fs +ucode-mod-uci +uclient-fetch +ca-bundle
  # Deliberately no CONFLICTS with mosdns: different package, UCI name and
  # path.  Port conflicts are reported at runtime by /etc/init.d/rust-dns.
endef

define Package/rust-dns/description
  static musl rust-dns daemon with procd lifecycle, UCI -> JSON configuration
  generation, dnsmasq forwarding, firewall4 DNS hijack and a geodata updater.
  The LuCI frontend is packaged separately as luci-app-rust-dns.
endef

# conffiles: user configuration and rule files survive upgrades.  Runtime
# files under /var/etc, /var/rust-dns and /var/log are managed by the service.
define Package/rust-dns/conffiles
/etc/config/rust-dns
/etc/rust-dns/config_custom.yaml
/etc/rust-dns/rule/whitelist.txt
/etc/rust-dns/rule/blocklist.txt
/etc/rust-dns/rule/greylist.txt
/etc/rust-dns/rule/ddnslist.txt
/etc/rust-dns/rule/hosts.txt
/etc/rust-dns/rule/redirect.txt
/etc/rust-dns/rule/local-ptr.txt
/etc/rust-dns/rule/streaming.txt
/etc/rust-dns/rule/cloudflare-cidr.txt
endef

define Build/Compile
endef

define Package/rust-dns/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/rust-dns $(1)/usr/bin/rust-dns

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./files/rust-dns.init $(1)/etc/init.d/rust-dns

	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./files/rust-dns.config $(1)/etc/config/rust-dns

	$(INSTALL_DIR) $(1)/etc/rust-dns/rule
	$(INSTALL_CONF) ./files/config_custom.yaml $(1)/etc/rust-dns/config_custom.yaml
	$(INSTALL_CONF) ./files/rule/whitelist.txt $(1)/etc/rust-dns/rule/whitelist.txt
	$(INSTALL_CONF) ./files/rule/blocklist.txt $(1)/etc/rust-dns/rule/blocklist.txt
	$(INSTALL_CONF) ./files/rule/greylist.txt $(1)/etc/rust-dns/rule/greylist.txt
	$(INSTALL_CONF) ./files/rule/ddnslist.txt $(1)/etc/rust-dns/rule/ddnslist.txt
	$(INSTALL_CONF) ./files/rule/hosts.txt $(1)/etc/rust-dns/rule/hosts.txt
	$(INSTALL_CONF) ./files/rule/redirect.txt $(1)/etc/rust-dns/rule/redirect.txt
	$(INSTALL_CONF) ./files/rule/local-ptr.txt $(1)/etc/rust-dns/rule/local-ptr.txt
	$(INSTALL_CONF) ./files/rule/streaming.txt $(1)/etc/rust-dns/rule/streaming.txt
	$(INSTALL_CONF) ./files/rule/cloudflare-cidr.txt $(1)/etc/rust-dns/rule/cloudflare-cidr.txt

	$(INSTALL_DIR) $(1)/usr/share/rust-dns
	$(INSTALL_BIN) ./files/config.sh $(1)/usr/share/rust-dns/config.sh
	$(INSTALL_BIN) ./files/rust-dns.uc $(1)/usr/share/rust-dns/rust-dns.uc
	$(INSTALL_DATA) ./files/nft-hijack.nft $(1)/usr/share/rust-dns/nft-hijack.nft
	$(INSTALL_DATA) ./files/dnsmasq-rust-dns.conf $(1)/usr/share/rust-dns/dnsmasq-rust-dns.conf

	$(INSTALL_DIR) $(1)/etc/hotplug.d/iface
	$(INSTALL_BIN) ./files/hotplug-iface $(1)/etc/hotplug.d/iface/95-rust-dns

	$(INSTALL_DIR) $(1)/etc/uci-defaults
	$(INSTALL_BIN) ./files/uci-defaults/90-rust-dns $(1)/etc/uci-defaults/90-rust-dns
endef

define Package/rust-dns/postinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] || {
	mkdir -p /etc/rust-dns/rule /var/etc/rust-dns /var/rust-dns
	chmod 0755 /etc/rust-dns /etc/rust-dns/rule /var/etc/rust-dns /var/rust-dns 2>/dev/null
	chmod 0644 /etc/rust-dns/config_custom.yaml /etc/rust-dns/rule/*.txt 2>/dev/null
	chmod 0755 /etc/init.d/rust-dns /usr/share/rust-dns/config.sh \
		/usr/share/rust-dns/rust-dns.uc /etc/hotplug.d/iface/95-rust-dns 2>/dev/null
}
# Do not enable or start the service automatically; the user decides.
exit 0
endef

define Package/rust-dns/prerm
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] || {
	[ -x /etc/init.d/rust-dns ] && /etc/init.d/rust-dns stop >/dev/null 2>&1
	rm -f /tmp/dnsmasq.d/rust-dns.conf
	rm -f /usr/share/nftables.d/chain-post/dstnat/99-rust-dns.nft
	sed -i '/^# BEGIN rust-dns$$/,/^# END rust-dns$$/d' /etc/crontabs/root 2>/dev/null
	[ -x /etc/init.d/dnsmasq ] && /etc/init.d/dnsmasq reload >/dev/null 2>&1
	command -v fw4 >/dev/null 2>&1 && fw4 reload >/dev/null 2>&1
}
exit 0
endef

define Package/rust-dns/postrm
#!/bin/sh
echo "rust-dns: user configuration (/etc/config/rust-dns, /etc/rust-dns)"
echo "rust-dns: and data (/var/rust-dns, /var/log/rust-dns.log) were preserved."
exit 0
endef

$(eval $(call BuildPackage,rust-dns))
