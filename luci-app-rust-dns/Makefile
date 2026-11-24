include $(TOPDIR)/rules.mk

PKG_NAME:=luci-app-rust-dns
PKG_VERSION:=0.1.1
PKG_RELEASE:=1

LUCI_TITLE:=LuCI Support for rust-dns
LUCI_DESCRIPTION:=LuCI configuration, rules, statistics and log pages for rust-dns
LUCI_PKGARCH:=all
# The rpcd ucode backend imports the ucode `fs` and `uci` modules and is run
# by rpcd's ucode plugin, so depend on them explicitly instead of relying on
# luci-base to pull them in transitively.
LUCI_DEPENDS:=+rust-dns +ucode +ucode-mod-fs +ucode-mod-uci +rpcd-mod-ucode +uclient-fetch

PKG_MAINTAINER:=rust-dns contributors
PKG_LICENSE:=GPL-3.0-only

# /etc/config/rust-dns, config_custom.yaml and rule files are owned by the
# rust-dns service package; this LuCI package must not install duplicates.

# rpcd loads ucode handlers only at startup/reload, so an install or upgrade
# must restart it (and uhttpd for the menu/rpc cache) or the rust-dns RPC
# object is missing until the next reboot.
define Package/$(PKG_NAME)/postinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] || {
	rm -f /tmp/luci-indexcache.*
	rm -rf /tmp/luci-modulecache/
	[ -x /etc/init.d/rpcd ] && /etc/init.d/rpcd restart >/dev/null 2>&1
	[ -x /etc/init.d/uhttpd ] && /etc/init.d/uhttpd restart >/dev/null 2>&1
}
exit 0
endef

include $(TOPDIR)/feeds/luci/luci.mk

# call BuildPackage - OpenWrt buildroot signature
