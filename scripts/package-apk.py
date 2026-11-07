#!/usr/bin/env python3
"""Assembles an unsigned, unaligned APK from aapt2 output, a dex and native libs.

usage: package-apk.py BASE.apk OUT.apk classes.dex LIBDIR [ABI ...]

Native libraries are stored uncompressed so they can be mmapped straight from
the APK (android:extractNativeLibs="false"); run zipalign -P 16 afterwards.
"""
import os
import sys
import zipfile

base, out, dex, libdir, *abis = sys.argv[1:]
with zipfile.ZipFile(base) as src, zipfile.ZipFile(out, "w") as dst:
    # aapt2 output first: AndroidManifest.xml, resources.arsc (stored), res/...
    for info in src.infolist():
        dst.writestr(info, src.read(info.filename))
    dst.write(dex, "classes.dex", compress_type=zipfile.ZIP_DEFLATED)
    for abi in abis:
        d = os.path.join(libdir, abi)
        for name in sorted(os.listdir(d)):
            if name.endswith(".so"):
                dst.write(os.path.join(d, name), f"lib/{abi}/{name}", compress_type=zipfile.ZIP_STORED)
