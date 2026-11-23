include $(TOPDIR)/rules.mk

PKG_NAME:=luci-app-rust-dns
PKG_VERSION:=0.1.0
PKG_RELEASE:=1

LUCI_TITLE:=LuCI Support for rust-dns
LUCI_DESCRIPTION:=LuCI configuration, rules, statistics and log pages for rust-dns
LUCI_PKGARCH:=all
# The rpcd ucode backend imports the ucode `fs` and `uci` modules, so depend on
# them explicitly instead of relying on luci-base to pull them in transitively.
LUCI_DEPENDS:=+rust-dns +ucode +ucode-mod-fs +ucode-mod-uci +uclient-fetch

PKG_MAINTAINER:=rust-dns contributors
PKG_LICENSE:=GPL-3.0-only

# /etc/config/rust-dns, config_custom.yaml and rule files are owned by the
# rust-dns service package; this LuCI package must not install duplicates.
include $(TOPDIR)/feeds/luci/luci.mk

# call BuildPackage - OpenWrt buildroot signature
