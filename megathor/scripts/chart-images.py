#!/usr/bin/env python3
"""Print the images in rendered Kubernetes manifests (helm template output on stdin).

Each image is printed as a "<source image> <path in a mirror registry>" line, the format
scripts/mirror-images.bash reads; the path drops the registry host (and Docker Hub's "library/"),
matching how megathor's offline settings and the charts' global.imageRegistry address the mirror.
Besides every "image" field, ConfigMap values under keys ending in _IMAGE are included, since
operators such as Rook read the images they deploy (the Ceph CSI drivers) from a ConfigMap.

    helm template ... | scripts/chart-images.py
"""
import sys

import yaml


def walk(node, images):
    """Collect every non-empty string "image" field below a manifest node"""
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "image" and isinstance(value, str) and value:
                images.add(value)
            else:
                walk(value, images)
    elif isinstance(node, list):
        for item in node:
            walk(item, images)


def mirror_path(image):
    """The path an image is mirrored under: its reference without the registry host"""
    parts = image.split("/")
    if len(parts) > 1 and ("." in parts[0] or ":" in parts[0] or parts[0] == "localhost"):
        parts = parts[1:]
    if len(parts) > 1 and parts[0] == "library":
        parts = parts[1:]
    return "/".join(parts)


def main():
    """Print the images found in the manifests on stdin"""
    images = set()
    for doc in yaml.safe_load_all(sys.stdin):
        if not isinstance(doc, dict):
            continue
        if doc.get("kind") == "ConfigMap":
            for key, value in (doc.get("data") or {}).items():
                if key.endswith("_IMAGE") and isinstance(value, str) and value.strip():
                    images.add(value.strip())
            continue
        walk(doc, images)
    for image in sorted(images):
        print(image, mirror_path(image))


if __name__ == "__main__":
    main()
