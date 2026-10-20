#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Regenerates src/cloner/data/ from a thingino-dfu checkout:
#   tools/gen-cloner-data/gen.sh /path/to/thingino-dfu-historical
# Needs gcc, python3 and a built libtdfu (make in the thingino-dfu tree).
set -eu
src=${1:?usage: gen.sh <thingino-dfu dir>}
here=$(cd "$(dirname "$0")" && pwd)
out=$here/../../src/cloner/data
mkdir -p "$out"
gcc -O1 -I"$src/libtdfu/include" -I"$src/libtdfu/src" -o "$here/gen" "$here/gen.c" \
	"$src/build/libtdfu/libtdfu.a" -lusb-1.0 -lz -lm -lpthread
"$here/gen" "$src/firmware" "$out"
rm -f "$here/gen"
cp "$src/firmware/cloner/t31x/spl.bin" "$out/spl.bin"
cp "$src/firmware/cloner/t31x/uboot.bin" "$out/uboot.bin"
python3 - "$src/libtdfu/src/usb/protocol.c" "$out/detect.bin" <<'EOF'
import re, sys
text = open(sys.argv[1]).read()
body = text.split("static const uint8_t mips_prog[] = {", 1)[1].split("\n    };", 1)[0]
body = re.sub(r"/\*.*?\*/", "", body, flags=re.S)
open(sys.argv[2], "wb").write(bytes(int(x, 16) for x in re.findall(r"0x([0-9A-Fa-f]{2})", body)))
EOF
ls -l "$out"
