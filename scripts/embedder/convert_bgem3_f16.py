#!/usr/bin/env python3
"""Convert canonical BAAI/bge-m3 fp32 pytorch_model.bin -> f16 model.safetensors.

Provenance-first: verifies the source sha256 against the K1-pinned value before
converting, casts only floating-point tensors to f16, and proves the output is a
pure cast by bitwise-comparing every tensor after reload. Prints sha256 of the output.
"""
import hashlib, json, os, sys, glob

import torch
from safetensors.torch import save_file, load_file

PINNED_SRC_SHA = "b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38"
PINNED_REV = "5617a9f61b028005a4858fdac845db406aefb181"

def sha256(path, bufsize=1 << 22):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(bufsize):
            h.update(chunk)
    return h.hexdigest()

snap = glob.glob(os.path.expanduser(
    f"~/.cache/huggingface/hub/models--BAAI--bge-m3/snapshots/{PINNED_REV}*"))
if not snap:
    snap = glob.glob(os.path.expanduser(
        "~/.cache/huggingface/hub/models--BAAI--bge-m3/snapshots/*"))
assert snap, "no cached BAAI/bge-m3 snapshot"
src = os.path.join(snap[0], "pytorch_model.bin")
print(f"source: {src}", flush=True)

print("hashing source ...", flush=True)
src_sha = sha256(src)
print(f"  sha256 {src_sha}", flush=True)
assert src_sha == PINNED_SRC_SHA, "SOURCE HASH MISMATCH — refusing to convert"

print("loading state dict (fp32 pickle, ~2.3 GB) ...", flush=True)
sd = torch.load(src, map_location="cpu", weights_only=True)
if not isinstance(sd, dict):
    sd = sd.state_dict()

out_sd, kept, cast = {}, 0, 0
for name, t in sd.items():
    if t.is_floating_point():
        out_sd[name] = t.to(torch.float16).clone().contiguous()
        cast += 1
    else:
        out_sd[name] = t.clone().contiguous()
        kept += 1
print(f"tensors: {len(sd)} total, {cast} cast fp->f16, {kept} kept as-is", flush=True)

out_dir = sys.argv[1]
os.makedirs(out_dir, exist_ok=True)
out = os.path.join(out_dir, "model.safetensors")
save_file(out_sd, out, metadata={"format": "pt"})
print(f"wrote {out} ({os.path.getsize(out):,} B)", flush=True)

print("verifying: bitwise compare every tensor against a fresh cast ...", flush=True)
back = load_file(out)
assert set(back) == set(sd), "tensor name set mismatch"
bad = 0
for name, t in sd.items():
    want = t.to(torch.float16) if t.is_floating_point() else t
    got = back[name]
    if got.shape != want.shape or got.dtype != want.dtype or not torch.equal(got, want):
        print(f"  MISMATCH {name}"); bad += 1
assert bad == 0, f"{bad} tensors mismatched"
print(f"  all {len(sd)} tensors bitwise-identical to f16 cast of canonical source", flush=True)
print(f"output sha256 {sha256(out)}", flush=True)
