#!/bin/bash
# 把 Collaplex RT 打成 deb（预编译二进制 + 素材 + PipeWire 模板）
#
#   bash build-deb.sh              读 VERSION
#   VER=0.1.1 bash build-deb.sh    临时指定版本
#
# 产物: dist/collaplex-rt_<版本>_amd64.deb
# 装完不会自动接管；手动 collaplex-rt start。
set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
VER="${VER:-$(tr -d ' \t\n' < "$ROOT/VERSION")}"
OUT="$ROOT/dist"
PKG="collaplex-rt_${VER}_amd64"
STAGE="$OUT/$PKG"

[ -x "$ROOT/target/release/collaplex-realtime" ] || {
    echo "先构建: cargo build --release" >&2
    exit 1
}
[ -x "$ROOT/target/release/rt-web" ] || {
    echo "缺少 target/release/rt-web" >&2
    exit 1
}

echo "=== 打包 collaplex-rt $VER ==="
rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/collaplex-rt" \
         "$STAGE/usr/share/collaplex-rt/scripts"

install -m 755 "$ROOT/target/release/collaplex-realtime" "$STAGE/usr/bin/collaplex-realtime"
install -m 755 "$ROOT/target/release/rt-web" "$STAGE/usr/bin/collaplex-rt-web"
install -m 755 "$ROOT/packaging/collaplex-rt" "$STAGE/usr/bin/collaplex-rt"
install -m 755 "$ROOT/packaging/collaplex-rt-dsp" "$STAGE/usr/bin/collaplex-rt-dsp"
install -m 755 "$ROOT/packaging/collaplex-rt-gap" "$STAGE/usr/bin/collaplex-rt-gap"
install -m 755 "$ROOT/packaging/collaplex-rt-gap-r" "$STAGE/usr/bin/collaplex-rt-gap-r"
install -m 755 "$ROOT/packaging/install.sh" "$STAGE/usr/share/collaplex-rt/install.sh"
install -m 755 "$ROOT/scripts/chain-lib.sh" "$STAGE/usr/share/collaplex-rt/scripts/chain-lib.sh"
install -m 755 "$ROOT/scripts/target-watch.sh" "$STAGE/usr/share/collaplex-rt/scripts/target-watch.sh"

cp -a "$ROOT/assets" "$ROOT/pipewire" "$STAGE/usr/share/collaplex-rt/"
cp "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/LICENSING.md" "$ROOT/NOTICE" "$ROOT/VERSION" "$STAGE/usr/share/collaplex-rt/"
chmod -R a+rX "$STAGE/usr/share"

DOC="$STAGE/usr/share/doc/collaplex-rt"
mkdir -p "$DOC"
cat > "$DOC/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: collaplex-rt
Source: https://collaplex.me/git/edgevoid/collaplex-realtime

Files: *
Copyright: 2026 Collaplex
License: Apache-2.0
 本项目以 Apache-2.0 发布，允许商业使用；全文见 /usr/share/collaplex-rt/LICENSE，
 版权与第三方声明见 /usr/share/collaplex-rt/NOTICE。

Files: assets/hrir/*
Copyright: AudioLab, University of York
License: Apache-2.0
 SADIE II 数据集（受试者 H4），允许商用，保留其版权与许可声明。
 https://www.york.ac.uk/sadie-project/
EOF

SIZE=$(du -sk "$STAGE/usr" | cut -f1)
cat > "$STAGE/DEBIAN/control" <<EOF
Package: collaplex-rt
Version: ${VER}
Architecture: amd64
Maintainer: Collaplex <collaplex@localhost>
Installed-Size: ${SIZE}
Depends: libc6, libgcc-s1, pipewire (>= 1.0), pipewire-bin, wireplumber, python3
Section: sound
Priority: optional
Description: Collaplex RT — 实时 HRTF + 影院 IR + 按片段 AGC
 PipeWire convolver 负责 HRTF 和影院房间 IR、Rust DSP 负责 250 Hz 搁架、
 左右 8:2、按片段响度归一（干声锚点 + 房间传递模型 + 闭环响度上限）和 lookahead 限幅。
 .
 自带本地 Web 控制台（转盘/频谱/VU/示波器、实时参数）。装完不会自动接管，
 运行 collaplex-rt start 才启用，stop 即还原硬件输出。
EOF

(cd "$STAGE" && find usr -type f -exec md5sum {} + > DEBIAN/md5sums)
dpkg-deb --root-owner-group --build "$STAGE" "$OUT/${PKG}.deb" >/dev/null
echo "已生成: $OUT/${PKG}.deb"
dpkg-deb --info "$OUT/${PKG}.deb" | sed -n '1,14p'
echo
echo -n "大小: "; du -h "$OUT/${PKG}.deb" | cut -f1
