include $(TOPDIR)/rules.mk

PKG_NAME:=luci-app-rust-dns
PKG_VERSION:=0.1.3
PKG_RELEASE:=1

LUCI_TITLE:=LuCI Support for rust-dns
LUCI_DESCRIPTION:=LuCI configuration, rules, statistics and log pages for rust-dns
LUCI_PKGARCH:=all
# luci-base provides the JS framework (baseclass/form/rpc); the rpcd ucode
# backend imports the ucode `fs` and `uci` modules and is run by rpcd's ucode
# plugin, so depend on them explicitly instead of relying on transitive pulls.
LUCI_DEPENDS:=+luci-base +rust-dns +ucode +ucode-mod-fs +ucode-mod-uci +rpcd-mod-ucode +uclient-fetch

# LuCI's jsmin step leaves a stray <file>.js.o behind when it cannot minify an
# already minified file (js-yaml).  Turn the step off so the package only
# contains the files from htdocs.
LUCI_MINIFY_JS:=0

PKG_MAINTAINER:=rust-dns contributors
PKG_LICENSE:=GPL-3.0-only

# /etc/config/rust-dns, config_custom.yaml and rule files are owned by the
# rust-dns service package; this LuCI package must not install duplicates.

# rpcd loads ucode handlers only at startup/reload, so an install or upgrade
# must restart it (and uhttpd for the menu/rpc cache) or the rust-dns RPC
# object is missing until the next reboot.
#
# Keep the plain `postinst` variable name: package-pack.mk/BuildIPKGVariable
# reads it and writes CONTROL/postinst-pkg, which default_postinst() sources.
define Package/$(PKG_NAME)/postinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] || {
	# rpcd ignores world-writable ucode scripts, so an installer that
	# preserves odd modes (cp -pR from a 0777 feed) would silently disable
	# the whole luci.rust_dns object.  Normalize the files it loads.
	chmod 0644 /usr/share/rpcd/ucode/luci.rust_dns 2>/dev/null
	chmod 0644 /usr/share/rpcd/acl.d/luci-app-rust-dns.json 2>/dev/null
	chmod 0644 /usr/share/luci/menu.d/luci-app-rust-dns.json 2>/dev/null
	rm -f /tmp/luci-indexcache.*
	rm -rf /tmp/luci-modulecache/
	[ -x /etc/init.d/rpcd ] && /etc/init.d/rpcd restart >/dev/null 2>&1
	[ -x /etc/init.d/uhttpd ] && /etc/init.d/uhttpd restart >/dev/null 2>&1
}
exit 0
endef

include $(TOPDIR)/feeds/luci/luci.mk

# call BuildPackage - OpenWrt buildroot signature
